//! An MCP (Model Context Protocol) server over stdio that wraps the agentic
//! application's own HTTP surface -- the thing that makes it "fully agentic
//! native" rather than something an agent can only reach by shelling out to
//! curl.
//!
//! This process is a thin, stateless translator, not a second source of
//! truth: it asks the running app's own `GET /` for the current action
//! registry on every `tools/list` call, and turns each `Action` into an MCP
//! tool whose `inputSchema` is built from that same `Action`'s `params`.
//! Add a capability to the app's own registry and it appears here
//! automatically, with no change to this file -- the alternative, a second
//! hand-maintained tool list here, is exactly the kind of drift we avoid for
//! its call sites, and duplicating that mistake in a second process would
//! be worse, not better.
//!
//! This was originally a Python prototype (stdlib only, no `pip install`).
//! Rewritten in Rust so a Rust project never has to shell out to a second
//! language runtime just to talk to itself.
//!
//! Protocol: JSON-RPC 2.0, one message per line, over stdin/stdout -- MCP's
//! own stdio transport. Diagnostics go to stderr, never stdout, because
//! stdout is the wire.
//!
//! Usage (registered in `.mcp.json`):
//! ```text
//! AGENTIC_HTTP_PORT=7330 /path/to/target/release/agentic_mcp_server
//! ```
//! The port must match whatever HTTP port the app itself was launched
//! with -- this binary only speaks to that port, never picks one.

use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2024-11-05";

fn port() -> u16 {
    std::env::var("AGENTIC_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7330)
}

fn log(message: &str) {
    eprintln!("[agentic_mcp_server] {message}");
}

/// One HTTP call to the app's own HTTP surface. Hand-rolled over a raw
/// `TcpStream` rather than an HTTP client crate: every call here is a
/// single localhost round trip -- connect, write one request with
/// `Connection: close`, read until the peer actually closes, done. Raises
/// (returns `Err`) on any failure; the caller decides how that becomes an
/// MCP error, this function's only job is talking to the socket.
fn app_request(method: &str, path: &str, body: &str) -> Result<Value, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port())).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if body.is_empty() {
        request.push_str("Content-Length: 0\r\n\r\n");
    } else {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        request.push_str(body);
    }
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
    let response = String::from_utf8_lossy(&response);
    let response_body = response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(response_body).map_err(|e| format!("bad JSON response: {e}"))
}

/// `Param::kind` is a free-form string today ("string"/"integer"/"array"),
/// not an enum -- this is the one place that vocabulary has to agree with
/// JSON Schema's own, so an unrecognised kind falls back to "string" rather
/// than producing an invalid schema. A real consumer (exchanger, whose
/// `paths` array-param action worked end to end over HTTP but would have
/// failed a strict MCP client's own schema validation, since "array" fell
/// through to "string" here) is what caught `"array"` missing from this
/// list -- add any further JSON Schema primitive a real `Param::kind`
/// starts using, the same way.
fn json_schema_type(kind: &str) -> &str {
    match kind {
        "string" | "integer" | "boolean" | "number" | "array" | "object" => kind,
        _ => "string",
    }
}

/// Turns one `Action` (as the JSON `GET /` already returns it) into one MCP
/// tool. Every param becomes a flat top-level argument regardless of its
/// `location` -- an MCP client has no notion of path/query/body, that
/// distinction only matters once [`build_request`] turns arguments back
/// into an actual HTTP call.
fn action_to_tool(action: &Value) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in action["params"].as_array().into_iter().flatten() {
        let name = param["name"].as_str().unwrap_or_default();
        properties.insert(
            name.to_string(),
            json!({
                "type": json_schema_type(param["kind"].as_str().unwrap_or("string")),
                "description": param["description"],
            }),
        );
        required.push(json!(name));
    }
    json!({
        "name": action["name"],
        "description": format!(
            "{} ({} {})",
            action["description"].as_str().unwrap_or_default(),
            action["method"].as_str().unwrap_or_default(),
            action["path"].as_str().unwrap_or_default(),
        ),
        "inputSchema": {
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
        },
    })
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Minimal percent-encoding for a query-string value -- mirrors
/// [`gpui_agentic_http`]'s own `parse_query_string` decoder on the way
/// back, so this binary and the crate it talks to agree on the wire
/// format without either depending on a URL-encoding crate.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Builds the actual request path, query string, and JSON body for one
/// tool call, honoring each param's own `location`.
///
/// **This is the one real behavior change from the Python prototype it
/// replaces**, not just a language port: that version only ever
/// substituted every argument into the path template
/// (`path.replace("{name}", ...)`), which silently dropped any argument
/// whose param was actually `Query`- or `Body`-located -- there is no
/// `{name}` literal in the path for those, so the substitution was a
/// silent no-op. That bug would have made most of this app's mutation
/// actions (assign-bookings, reorder-stops, ...) uncallable through MCP
/// even after `dispatch` itself was fixed, since almost all of them carry
/// their real arguments in a `Body`, not the path. Fixed here rather than
/// reproduced.
fn build_request(action: &Value, arguments: &Value) -> Result<(String, String), String> {
    let mut path = action["path"].as_str().unwrap_or_default().to_string();
    let mut query_parts = Vec::new();
    let mut body_obj = serde_json::Map::new();

    for param in action["params"].as_array().into_iter().flatten() {
        let name = param["name"].as_str().unwrap_or_default();
        let value = arguments
            .get(name)
            .ok_or_else(|| format!("missing required argument: {name}"))?;
        match param["location"].as_str().unwrap_or("path") {
            "path" => {
                let placeholder = format!("{{{name}}}");
                path = path.replace(&placeholder, &value_to_string(value));
            }
            "query" => {
                query_parts.push(format!("{name}={}", percent_encode(&value_to_string(value))));
            }
            "body" => {
                body_obj.insert(name.to_string(), value.clone());
            }
            other => return Err(format!("unknown param location: {other}")),
        }
    }

    if !query_parts.is_empty() {
        path.push('?');
        path.push_str(&query_parts.join("&"));
    }
    let body = if body_obj.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&Value::Object(body_obj)).map_err(|e| e.to_string())?
    };
    Ok((path, body))
}

fn handle_initialize(params: &Value) -> Value {
    json!({
        "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION),
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "agentic-app", "version": "0.1.0"},
    })
}

fn handle_tools_list() -> Value {
    match app_request("GET", "/", "") {
        // No tools rather than an error: a client that lists tools before
        // the app is even up should see "nothing yet", not a broken
        // connection it has no way to act on.
        Err(e) => {
            log(&format!(
                "could not reach 127.0.0.1:{} -- is the app running with AGENTIC_HTTP_PORT={}? ({e})",
                port(),
                port(),
            ));
            json!({"tools": []})
        }
        Ok(capabilities) => {
            let tools: Vec<Value> = capabilities["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(action_to_tool)
                .collect();
            json!({"tools": tools})
        }
    }
}

fn handle_tools_call(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let empty_args = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty_args);

    let capabilities = match app_request("GET", "/", "") {
        Ok(c) => c,
        Err(e) => return error_result(&format!("could not reach the app: {e}")),
    };
    let Some(action) = capabilities["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"] == name)
    else {
        return error_result(&format!("no such action: {name}"));
    };

    let (path, body) = match build_request(action, arguments) {
        Ok(pair) => pair,
        Err(e) => return error_result(&e),
    };

    let method = action["method"].as_str().unwrap_or("GET");
    match app_request(method, &path, &body) {
        Ok(result) => json!({
            "content": [{"type": "text", "text": result.to_string()}],
            "isError": false,
        }),
        Err(e) => error_result(&format!("{method} {path} failed: {e}")),
    }
}

fn error_result(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

/// Dispatches one already-parsed JSON-RPC request. `Err` carries the
/// unrecognised method name -- the only failure mode left once every
/// handler above answers its own errors as an MCP `isError` result rather
/// than propagating one.
fn handle(method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "initialize" => Ok(handle_initialize(params)),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => Ok(handle_tools_call(params)),
        "ping" => Ok(json!({})),
        other => Err(other.to_string()),
    }
}

/// Parses one wire line and produces the response line to write, or `None`
/// for a line that gets no reply: a blank line, unparseable JSON (logged
/// and dropped), or a JSON-RPC *notification* (no `id` -- `notifications/
/// initialized` is the one every client sends, and this server has no
/// state to update on it).
fn process_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("bad JSON-RPC line, dropped: {e}"));
            return None;
        }
    };
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or_default();
    let empty = json!({});
    let params = request.get("params").unwrap_or(&empty);

    Some(match handle(method, params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(method) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {method}")},
        }),
    })
}

fn main() {
    log(&format!("starting, will talk to 127.0.0.1:{}", port()));
    let stdout = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Some(response) = process_line(&line) {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{response}");
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognised_kind_passes_through() {
        assert_eq!(json_schema_type("integer"), "integer");
    }

    #[test]
    fn an_unrecognised_kind_falls_back_to_string() {
        assert_eq!(json_schema_type("frobnicate"), "string");
    }

    /// Regression test: an array-kind param (e.g. exchanger's
    /// `POST /transfers`' `paths`) advertised its `inputSchema` type as
    /// "string" until this was caught by a real consumer -- the array
    /// still forwarded correctly over HTTP either way (`build_request`
    /// never consulted `kind`), so this only mattered to a strict MCP
    /// client validating arguments against the advertised schema before
    /// sending, not to the wire behavior itself.
    #[test]
    fn an_array_kind_is_advertised_as_an_array_not_a_string() {
        assert_eq!(json_schema_type("array"), "array");
    }

    #[test]
    fn an_object_kind_is_advertised_as_an_object_not_a_string() {
        assert_eq!(json_schema_type("object"), "object");
    }

    fn sample_action() -> Value {
        json!({
            "name": "assign_bookings",
            "method": "POST",
            "path": "/assign-bookings",
            "description": "assign bookings to a vehicle",
            "params": [
                {"name": "vehicle_id", "kind": "integer", "location": "path", "description": "which vehicle"},
                {"name": "verbose", "kind": "boolean", "location": "query", "description": "extra detail"},
                {"name": "booking_uids", "kind": "array", "location": "body", "description": "which bookings"},
            ],
        })
    }

    #[test]
    fn action_to_tool_flattens_every_param_regardless_of_location() {
        let tool = action_to_tool(&sample_action());
        assert_eq!(tool["name"], "assign_bookings");
        assert_eq!(
            tool["description"],
            "assign bookings to a vehicle (POST /assign-bookings)"
        );
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(props.len(), 3);
        assert_eq!(props["vehicle_id"]["type"], "integer");
        assert_eq!(props["verbose"]["type"], "boolean");
        assert_eq!(props["booking_uids"]["type"], "array");
        let required = tool["inputSchema"]["required"].as_array().unwrap();
        assert_eq!(required.len(), 3);
    }

    #[test]
    fn a_path_param_is_substituted_into_the_path_template() {
        let action = json!({
            "path": "/vehicle/{id}",
            "params": [{"name": "id", "location": "path"}],
        });
        let (path, body) = build_request(&action, &json!({"id": 42})).unwrap();
        assert_eq!(path, "/vehicle/42");
        assert_eq!(body, "");
    }

    #[test]
    fn a_query_param_is_appended_as_a_query_string() {
        let action = json!({
            "path": "/vehicles",
            "params": [{"name": "simulation_id", "location": "query"}],
        });
        let (path, body) = build_request(&action, &json!({"simulation_id": 7})).unwrap();
        assert_eq!(path, "/vehicles?simulation_id=7");
        assert_eq!(body, "");
    }

    #[test]
    fn a_query_param_value_is_percent_encoded() {
        let action = json!({
            "path": "/search",
            "params": [{"name": "q", "location": "query"}],
        });
        let (path, _) = build_request(&action, &json!({"q": "a b/c"})).unwrap();
        assert_eq!(path, "/search?q=a%20b%2Fc");
    }

    #[test]
    fn multiple_query_params_are_joined_with_ampersand() {
        let action = json!({
            "path": "/bookings",
            "params": [
                {"name": "simulation_id", "location": "query"},
                {"name": "state", "location": "query"},
            ],
        });
        let (path, _) =
            build_request(&action, &json!({"simulation_id": 1, "state": "assigned"})).unwrap();
        assert_eq!(path, "/bookings?simulation_id=1&state=assigned");
    }

    #[test]
    fn a_body_param_ends_up_as_a_json_body_not_in_the_path() {
        let action = json!({
            "path": "/assign-bookings",
            "params": [{"name": "booking_uids", "location": "body"}],
        });
        let (path, body) =
            build_request(&action, &json!({"booking_uids": ["a", "b"]})).unwrap();
        assert_eq!(path, "/assign-bookings");
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["booking_uids"], json!(["a", "b"]));
    }

    #[test]
    fn a_missing_required_argument_is_an_error() {
        let action = json!({
            "path": "/vehicle/{id}",
            "params": [{"name": "id", "location": "path"}],
        });
        let result = build_request(&action, &json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("id"));
    }

    #[test]
    fn ping_is_answered_directly_with_no_network_call() {
        assert_eq!(handle("ping", &json!({})).unwrap(), json!({}));
    }

    #[test]
    fn an_unknown_method_is_an_error_naming_it() {
        let err = handle("not/a/real/method", &json!({})).unwrap_err();
        assert_eq!(err, "not/a/real/method");
    }

    #[test]
    fn initialize_echoes_back_the_requested_protocol_version() {
        let result = handle_initialize(&json!({"protocolVersion": "2099-01-01"}));
        assert_eq!(result["protocolVersion"], "2099-01-01");
        assert_eq!(result["serverInfo"]["name"], "agentic-app");
    }

    #[test]
    fn initialize_falls_back_to_the_known_protocol_version() {
        let result = handle_initialize(&json!({}));
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn a_blank_line_gets_no_response() {
        assert!(process_line("").is_none());
        assert!(process_line("   ").is_none());
    }

    #[test]
    fn unparseable_json_gets_no_response() {
        assert!(process_line("not json").is_none());
    }

    #[test]
    fn a_notification_with_no_id_gets_no_response() {
        let line = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
        assert!(process_line(&line).is_none());
    }

    #[test]
    fn a_request_for_ping_gets_a_matching_response() {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string();
        let response = process_line(&line).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn a_request_for_an_unknown_method_gets_a_json_rpc_error() {
        let line = json!({"jsonrpc": "2.0", "id": 2, "method": "bogus"}).to_string();
        let response = process_line(&line).unwrap();
        assert_eq!(response["id"], 2);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn a_string_id_round_trips_as_a_string() {
        let line = json!({"jsonrpc": "2.0", "id": "abc", "method": "ping"}).to_string();
        let response = process_line(&line).unwrap();
        assert_eq!(response["id"], "abc");
    }
}
