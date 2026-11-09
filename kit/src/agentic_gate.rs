//! The agentic port's own accept loop: what stands between a request and
//! a module's `dispatch`.
//!
//! `gpui-agentic-http`'s `AgenticServer::spawn` answers every routed call
//! `200`, an unrouted one a plain-text `404`, a wrong method the same
//! `404`, and reads no request header. On a port bound to 127.0.0.1 that
//! is not enough: any web page the user opens can `fetch()` it (CSRF) or
//! point a DNS name it controls at 127.0.0.1 (DNS rebinding) and drive or
//! read Studio. This loop keeps the crate's types (`ModuleRegistration`,
//! `AgenticCall`, the same `GET /` document and the same routing rules),
//! so the dispatch side is unchanged, and adds, before dispatch:
//!
//! - **Origin**: a request carrying an `Origin` header is refused (403)
//!   unless it is this port's own loopback origin or listed in
//!   `GPUI_MAP_AGENTIC_ALLOW_ORIGINS` (comma-separated). `Origin: null`
//!   (a sandboxed iframe, a `file:` page) is refused too: it is exactly
//!   what a page sends to hide where it came from. Clients that are not
//!   browsers (curl, scripts, MCP bridges) send no `Origin`.
//! - **Host**: when present it must name the loopback (`127.0.0.1`,
//!   `localhost`, `[::1]`, with this port or none). A rebinding page's
//!   requests carry its own host name and get 403.
//! - **Sec-Fetch-Site**: a browser's `cross-site` / `same-site` fetch is
//!   refused even when it sends no `Origin` (a plain `<img src>` GET).
//!
//! and, after it, real status codes: a JSON `{"error": ...}` (or
//! `"ok": false`) answer is 400, 404 or 409 by what it says (see
//! [`status_for`]); an unknown path is a JSON 404 with the closest paths;
//! a known path with the wrong method is 405 with `Allow`; a body over
//! [`MAX_BODY`] is 413.

use std::io::Read as _;
use std::sync::mpsc::{channel, Receiver};

use gpui_agentic_http::{AgenticCall, ModuleRegistration};
use serde_json::{json, Value};

/// Largest request body read (a story script is the largest real one).
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// The variable that lists extra origins a browser page may call from.
pub const ALLOW_ORIGINS_VAR: &str = "GPUI_MAP_AGENTIC_ALLOW_ORIGINS";

/// Why a request is refused before routing: `(status, JSON body)`.
pub fn refuse(origin: Option<&str>, host: Option<&str>, fetch_site: Option<&str>, port: u16, allowed: &[String]) -> Option<(u16, Value)> {
    let loopback_origin = |o: &str| {
        let o = o.trim_end_matches('/').to_ascii_lowercase();
        [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}"), format!("http://[::1]:{port}")].contains(&o)
    };
    if let Some(o) = origin.map(str::trim) {
        let listed = allowed.iter().any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(o.trim_end_matches('/')));
        if !(loopback_origin(o) || listed) {
            return Some((403, json!({
                "error": "origin refused",
                "origin": o,
                "hint": format!("this port answers local programs, not web pages: a request with a browser Origin is refused. A local tool sends no Origin header; to allow a page, add its origin to {ALLOW_ORIGINS_VAR}"),
            })));
        }
    }
    if let Some(h) = host.map(|h| h.trim().to_ascii_lowercase()) {
        // `name[:port]`, where an IPv6 name is bracketed: `[::1]:7334`.
        let (name, port_part) = match h.find(']') {
            Some(end) if h.starts_with('[') => (&h[..=end], h[end + 1..].strip_prefix(':')),
            _ => match h.rsplit_once(':') {
                Some((n, p)) => (n, Some(p)),
                None => (h.as_str(), None),
            },
        };
        let name_ok = matches!(name, "127.0.0.1" | "localhost" | "[::1]");
        let port_ok = port_part.is_none_or(|p| p == port.to_string());
        if !(name_ok && port_ok) {
            return Some((403, json!({
                "error": "host refused",
                "host": h,
                "hint": format!("call http://127.0.0.1:{port}/ (a request for another host name is how a DNS-rebinding page reaches this port)"),
            })));
        }
    }
    if let Some(site) = fetch_site.map(|s| s.trim().to_ascii_lowercase()) {
        if (site == "cross-site" || site == "same-site") && origin.is_none_or(|o| !allowed.iter().any(|a| a.eq_ignore_ascii_case(o))) {
            return Some((403, json!({"error": "cross-site request refused", "hint": "this port answers local programs, not web pages"})));
        }
    }
    None
}

/// The HTTP status a dispatch answer gets. A JSON object with an
/// `"error"` (or `"ok": false`) is a failure: 404 when it names
/// something unknown, 409 when the app is not in the state the call needs
/// (a page not open, no map yet), 503 while shutting down, else 400.
/// Everything else is 200.
pub fn status_for(answer: &Value) -> u16 {
    let Some(obj) = answer.as_object() else { return 200 };
    let error = obj.get("error").filter(|e| !e.is_null());
    let failed = error.is_some() || obj.get("ok") == Some(&Value::Bool(false));
    if !failed {
        return 200;
    }
    let text = error.map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).unwrap_or_default().to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&["shutting down", "no reply"]) {
        503
    } else if has(&["unknown", "not found", "no such", "does not exist", "no file"]) {
        404
    } else if has(&["not open", "no map is active", "not available", "not a gallery", "needs ", "not running", "not ready", "busy", "already", "in flight", "not signed", "sign in", "locked"]) {
        409
    } else {
        400
    }
}

/// What one request routes to.
pub enum Routed<T> {
    Call(AgenticCall<T>),
    /// The path is an action's, the method is not: its methods.
    WrongMethod(Vec<&'static str>),
    NotFound,
}

/// The crate's routing rules (`/{prefix}{action.path}`, one `{name}`
/// placeholder per path), plus telling a wrong method from no route.
pub fn route<T>(modules: &[ModuleRegistration<T>], method: &str, path: &str, query: &str, body: &str) -> Routed<T> {
    let mut methods = Vec::new();
    for module in modules {
        let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(module.prefix)).and_then(|r| r.strip_prefix('/')) else {
            continue;
        };
        for action in module.actions {
            let action_path = action.path.strip_prefix('/').unwrap_or(action.path);
            let template = action_path.split('{').next().unwrap_or(action_path).trim_end_matches('/');
            let placeholder = action_path.contains('{');
            let matched = if placeholder {
                rest.strip_prefix(template).and_then(|r| r.strip_prefix('/')).filter(|r| !r.is_empty())
            } else if rest == template {
                Some("")
            } else {
                None
            };
            let Some(value) = matched else { continue };
            if action.method != method {
                if !methods.contains(&action.method) {
                    methods.push(action.method);
                }
                continue;
            }
            let (reply, _) = channel();
            return Routed::Call(AgenticCall {
                action_name: action.name,
                path_param: placeholder.then(|| value.to_string()),
                query: query.to_string(),
                body: body.to_string(),
                dispatch: module.dispatch,
                reply,
            });
        }
    }
    if methods.is_empty() {
        Routed::NotFound
    } else {
        Routed::WrongMethod(methods)
    }
}

/// `GET /`'s document: the crate's shape (`app`, `actions` with
/// `{prefix}.{name}`, `/{prefix}{path}`, params), plus how to reach the
/// same actions over MCP.
pub fn capabilities<T>(app_name: &str, modules: &[ModuleRegistration<T>]) -> Value {
    let actions: Vec<Value> = modules
        .iter()
        .flat_map(|m| {
            m.actions.iter().map(move |a| {
                json!({
                    "name": format!("{}.{}", m.prefix, a.name),
                    "method": a.method,
                    "path": format!("/{}{}", m.prefix, a.path),
                    "description": a.description,
                    "params": a.params.iter().map(|p| json!({"name": p.name, "kind": p.kind, "location": p.location, "description": p.description})).collect::<Vec<_>>(),
                })
            })
        })
        .collect();
    json!({
        "app": app_name,
        "actions": actions,
        "errors": "an error answers 4xx/5xx with a JSON {error, hint}; 403 for a browser Origin or a non-loopback Host, 404 for an unknown path, 405 for a wrong method",
        "mcp": "the same actions as MCP tools: run this app's binary with --mcp (stdio), e.g. `claude mcp add swat-studio -- <binary> --mcp`",
    })
}

fn header<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn respond(request: tiny_http::Request, status: u16, body: &Value, extra: &[(&str, String)]) {
    let mut response = tiny_http::Response::from_string(body.to_string()).with_status_code(status);
    let mut headers = vec![("Content-Type", "application/json".to_string()), ("Cache-Control", "no-store".to_string())];
    headers.extend(extra.iter().cloned());
    for (k, v) in headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            response.add_header(h);
        }
    }
    let _ = request.respond(response);
}

fn allowed_origins() -> Vec<String> {
    std::env::var(ALLOW_ORIGINS_VAR).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// Binds 127.0.0.1:`port` and starts the loop. `None` when the port does
/// not bind, as the crate's `spawn`.
pub fn spawn<T: 'static>(app_name: &'static str, port: u16, modules: Vec<ModuleRegistration<T>>) -> Option<Receiver<AgenticCall<T>>> {
    let server = match tiny_http::Server::http(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentic http: could not bind 127.0.0.1:{port}: {e}");
            return None;
        }
    };
    Some(serve(server, app_name, modules))
}

/// The loop over an already bound server (tests bind port 0).
pub fn serve<T: 'static>(server: tiny_http::Server, app_name: &'static str, modules: Vec<ModuleRegistration<T>>) -> Receiver<AgenticCall<T>> {
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let (calls, receiver) = channel();
    let doc = capabilities(app_name, &modules);
    let allowed = allowed_origins();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            if let Some((status, body)) = refuse(header(&request, "Origin"), header(&request, "Host"), header(&request, "Sec-Fetch-Site"), port, &allowed) {
                respond(request, status, &body, &[]);
                continue;
            }
            let method = request.method().as_str().to_ascii_uppercase();
            let url = request.url().to_string();
            let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
            let mut bytes = Vec::new();
            let read = request.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut bytes);
            if read.is_err() || bytes.len() > MAX_BODY {
                respond(request, 413, &json!({"error": "body too large", "limit_bytes": MAX_BODY}), &[]);
                continue;
            }
            let body = String::from_utf8_lossy(&bytes).into_owned();
            if path == "/" || path.is_empty() {
                if method == "GET" {
                    respond(request, 200, &doc, &[]);
                } else {
                    respond(request, 405, &json!({"error": "method not allowed", "allow": ["GET"]}), &[("Allow", "GET".into())]);
                }
                continue;
            }
            match route(&modules, &method, path, query, &body) {
                Routed::Call(call) => {
                    let (reply, answer) = channel();
                    if calls.send(AgenticCall { reply, ..call }).is_err() {
                        respond(request, 503, &json!({"error": format!("{app_name} is shutting down")}), &[]);
                        continue;
                    }
                    let body = answer.recv().unwrap_or_else(|_| json!({"error": "no reply"}));
                    respond(request, status_for(&body), &body, &[]);
                }
                Routed::WrongMethod(allow) => {
                    let list = allow.join(", ");
                    respond(request, 405, &json!({"error": "method not allowed", "path": path, "allow": allow, "hint": format!("{path} takes {list}")}), &[("Allow", list)]);
                }
                Routed::NotFound => {
                    let paths: Vec<String> = modules.iter().flat_map(|m| m.actions.iter().map(move |a| format!("/{}{}", m.prefix, a.path))).collect();
                    let close = crate::agentic::did_you_mean(path.trim_start_matches('/'), &paths.iter().map(|p| p.trim_start_matches('/').to_string()).collect::<Vec<_>>(), 3);
                    respond(request, 404, &json!({"error": "no such route", "path": path, "did_you_mean": close.iter().map(|p| format!("/{p}")).collect::<Vec<_>>(), "hint": "GET / lists every action with its method, path and params"}), &[]);
                }
            }
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_agentic_http::{Action, Param, ParamLocation};
    use std::io::Write as _;

    struct Fake;

    const ACTIONS: &[Action] = &[
        Action { name: "state", method: "GET", path: "/state", description: "the state", params: &[] },
        Action { name: "open", method: "POST", path: "/open", description: "opens", params: &[Param { name: "id", kind: "string", location: ParamLocation::Body, description: "id" }] },
    ];

    fn dispatch(_: &mut Fake, name: &str, _: Option<&str>, _: &str, body: &str) -> Value {
        match name {
            "state" => json!({"page": "home"}),
            "open" if body.contains("bad") => json!({"ok": false, "error": "unknown id"}),
            "open" => json!({"ok": true}),
            _ => json!({"error": "unrouted"}),
        }
    }

    fn modules() -> Vec<ModuleRegistration<Fake>> {
        vec![ModuleRegistration { prefix: "ui", actions: ACTIONS, dispatch }]
    }

    /// A live loop on an ephemeral port, its calls answered by `dispatch`.
    fn live() -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("binds");
        let port = server.server_addr().to_ip().expect("ip").port();
        let rx = serve(server, "test-app", modules());
        std::thread::spawn(move || {
            let mut app = Fake;
            for call in rx {
                let v = (call.dispatch)(&mut app, call.action_name, call.path_param.as_deref(), &call.query, &call.body);
                let _ = call.reply.send(v);
            }
        });
        port
    }

    /// One raw HTTP/1.1 exchange: (status, body).
    fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connects");
        let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            req += &format!("Host: 127.0.0.1:{port}\r\n");
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        req += body;
        s.write_all(req.as_bytes()).expect("writes");
        let mut out = String::new();
        s.read_to_string(&mut out).expect("reads");
        let status = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    #[test]
    fn a_forged_origin_is_refused_with_403() {
        let port = live();
        let (st, body) = send(port, "POST", "/ui/open", &[("Origin", "https://evil.example")], r#"{"id":"x"}"#);
        assert_eq!(st, 403);
        assert_eq!(body["error"], "origin refused");
        let (st, _) = send(port, "GET", "/ui/state", &[("Origin", "null")], "");
        assert_eq!(st, 403, "Origin: null is a page hiding where it came from");
        let (st, _) = send(port, "GET", "/ui/state", &[("Sec-Fetch-Site", "cross-site")], "");
        assert_eq!(st, 403, "a cross-site browser GET with no Origin");
        let (st, _) = send(port, "GET", "/ui/state", &[("Origin", &format!("http://127.0.0.1:{port}"))], "");
        assert_eq!(st, 200, "the port's own loopback origin");
    }

    #[test]
    fn a_wrong_host_is_refused_with_403() {
        let port = live();
        let (st, body) = send(port, "GET", "/ui/state", &[("Host", "rebind.evil.example:7334")], "");
        assert_eq!(st, 403);
        assert_eq!(body["error"], "host refused");
        let (st, _) = send(port, "GET", "/ui/state", &[("Host", "127.0.0.1:1")], "");
        assert_eq!(st, 403, "another port's host");
        let (st, _) = send(port, "GET", "/ui/state", &[("Host", &format!("localhost:{port}"))], "");
        assert_eq!(st, 200);
    }

    #[test]
    fn a_wrong_method_is_405_with_allow() {
        let port = live();
        let (st, body) = send(port, "GET", "/ui/open", &[], "");
        assert_eq!(st, 405);
        assert_eq!(body["allow"], json!(["POST"]));
        let (st, _) = send(port, "POST", "/", &[], "");
        assert_eq!(st, 405);
    }

    #[test]
    fn a_tool_error_is_a_non_200_json_error() {
        let port = live();
        let (st, body) = send(port, "POST", "/ui/open", &[], r#"{"id":"bad"}"#);
        assert_eq!(st, 404, "an unknown id");
        assert_eq!(body["error"], "unknown id");
        let (st, body) = send(port, "POST", "/ui/open", &[], r#"{"id":"good"}"#);
        assert_eq!((st, body["ok"].clone()), (200, json!(true)));
        let (st, body) = send(port, "GET", "/ui/stat", &[], "");
        assert_eq!(st, 404);
        assert_eq!(body["did_you_mean"][0], "/ui/state");
        let (st, body) = send(port, "GET", "/", &[], "");
        assert_eq!(st, 200);
        assert_eq!(body["actions"][0]["name"], "ui.state");
    }

    #[test]
    fn status_codes_follow_what_the_error_says() {
        assert_eq!(status_for(&json!({"queued": true})), 200);
        assert_eq!(status_for(&json!({"error": "could not parse that call"})), 400);
        assert_eq!(status_for(&json!({"ok": false, "error": "unknown id"})), 404);
        assert_eq!(status_for(&json!({"error": "Planning → Datasets is not open (the sidebar's Datasets)"})), 409);
        assert_eq!(status_for(&json!({"error": "no map is active"})), 409);
        assert_eq!(status_for(&json!({"error": "measure must be routes, trips, speed"})), 400);
        assert_eq!(status_for(&json!([1, 2])), 200);
    }
}
