//! The transport: request routing across every registered
//! [`ModuleRegistration`], real query-string parsing, and the
//! `tiny_http` accept-loop/channel bridge -- ported near-verbatim from a
//! downstream gpui app's own `debug_http::spawn`, with the two hard-won,
//! measured-bug fixes it carried preserved here: the
//! channel-crossing pattern itself, and the forced-poll loop a consumer's
//! own app entity still has to wire up -- see this file's own doc comment
//! on [`AgenticServer::spawn`] for why that part cannot move into this
//! crate.

use crate::registry::ModuleRegistration;

/// A parsed, routed request, paired with where its JSON answer goes --
/// `debug_http.rs`'s own `DebugCall`, generalized over the app type `T`.
///
/// **Carries the resolved `dispatch` function pointer, not just the
/// parsed pieces.** `dispatch` needs `&mut T` (e.g. `&mut MyApp`),
/// which only exists on the *consumer's* own foreground thread -- the
/// HTTP accept-loop thread that does the routing never has one, so
/// routing alone cannot call it, only identify which one an incoming
/// request means. The consumer's own poll loop is what actually
/// calls `(call.dispatch)(app, ...)` once it has real `&mut T` in hand,
/// then sends the JSON result down `call.reply`.
pub struct AgenticCall<T> {
    pub action_name: &'static str,
    pub path_param: Option<String>,
    pub query: String,
    pub body: String,
    pub dispatch: fn(&mut T, &str, Option<&str>, &str, &str) -> serde_json::Value,
    pub reply: std::sync::mpsc::Sender<serde_json::Value>,
}

pub struct AgenticServer<T> {
    app_name: &'static str,
    port: u16,
    modules: Vec<ModuleRegistration<T>>,
}

impl<T: 'static> AgenticServer<T> {
    pub fn new(app_name: &'static str, port: u16) -> Self {
        Self {
            app_name,
            port,
            modules: Vec::new(),
        }
    }

    /// Registers one module, panicking in debug builds if its `prefix`
    /// collides with an already-registered one -- see
    /// [`ModuleRegistration::prefix`]'s own doc comment for why this is a
    /// `debug_assert!` rather than a compile-time check: two modules
    /// composed from independent crates have no shared type the compiler
    /// could check this against.
    pub fn register(mut self, registration: ModuleRegistration<T>) -> Self {
        debug_assert!(
            !self.modules.iter().any(|m| m.prefix == registration.prefix),
            "gpui-agentic-http: two modules registered the same prefix {:?} -- \
             pick a different namespace for one of them",
            registration.prefix,
        );
        self.modules.push(registration);
        self
    }

    /// `GET /`'s own body -- every registered module's actions, each
    /// reported under its `{prefix}.{name}` qualified name, plus the
    /// `"app"` field a port-range scanner reads to know which app this is
    /// (see the design spec's own "Discovery" section).
    fn capabilities_json(&self) -> serde_json::Value {
        let actions: Vec<serde_json::Value> = self
            .modules
            .iter()
            .flat_map(|m| {
                m.actions.iter().map(move |a| {
                    serde_json::json!({
                        "name": format!("{}.{}", m.prefix, a.name),
                        "method": a.method,
                        "path": format!("/{}{}", m.prefix, a.path),
                        "description": a.description,
                        "params": a.params.iter().map(|p| serde_json::json!({
                            "name": p.name,
                            "kind": p.kind,
                            "location": p.location,
                            "description": p.description,
                        })).collect::<Vec<_>>(),
                    })
                })
            })
            .collect();
        serde_json::json!({ "app": self.app_name, "actions": actions })
    }

    /// `(method, path, query, body)` to the module+action it names, or
    /// `None` for anything no registered module answers -- a 404, not a
    /// panic. Pure and free of `tiny_http`, the same reason
    /// `debug_http.rs`'s own `parse_request` was: testable without a real
    /// socket.
    /// Kept as an inherent method purely so `routing_tests` (below) can
    /// exercise routing against a live `AgenticServer` the way a consumer
    /// actually builds one; production code only ever calls
    /// [`AgenticServer::route_over`] directly (see `spawn`'s own doc
    /// comment on why).
    #[allow(dead_code)]
    fn route(&self, method: &str, path: &str, query: &str, body: &str) -> Option<AgenticCall<T>> {
        Self::route_over(&self.modules, method, path, query, body)
    }

    /// The actual routing logic, over a borrowed slice of modules rather
    /// than `&self` -- so [`AgenticServer::spawn`]'s accept-loop thread can
    /// call it against an owned `Vec<ModuleRegistration<T>>` it moved out
    /// of `self` before spawning, instead of trying to smuggle a `&self`
    /// borrow into a `'static` closure (`self` cannot outlive `spawn`).
    fn route_over(
        modules: &[ModuleRegistration<T>],
        method: &str,
        path: &str,
        query: &str,
        body: &str,
    ) -> Option<AgenticCall<T>> {
        if method == "GET" && path == "/" {
            // Handled by the caller directly against `capabilities_json`,
            // not routed through a module -- there is no module whose
            // prefix is empty.
            return None;
        }
        for module in modules {
            let Some(rest) = path
                .strip_prefix('/')
                .and_then(|p| p.strip_prefix(module.prefix))
            else {
                continue;
            };
            let Some(rest) = rest.strip_prefix('/') else {
                continue;
            };
            for action in module.actions {
                if action.method != method {
                    continue;
                }
                let action_path = action.path.strip_prefix('/').unwrap_or(action.path);
                let template_prefix = action_path
                    .split('{')
                    .next()
                    .unwrap_or(action_path)
                    .trim_end_matches('/');
                let has_placeholder = action_path.contains('{');
                let matched = if has_placeholder {
                    rest.strip_prefix(template_prefix)
                        .and_then(|r| r.strip_prefix('/'))
                        .filter(|r| !r.is_empty())
                } else if rest == template_prefix {
                    Some("")
                } else {
                    None
                };
                let Some(param_value) = matched else { continue };
                let path_param = if has_placeholder {
                    Some(param_value.to_string())
                } else {
                    None
                };
                // `reply` is a throwaway placeholder here -- `route` only
                // resolves *which* action and its params; `spawn`'s own
                // loop (Step 3) replaces this with the real one-shot
                // channel it actually waits on, the same way it already
                // has to replace other fields to attach the per-request
                // reply sender.
                let (reply, _unused) = std::sync::mpsc::channel();
                return Some(AgenticCall {
                    action_name: action.name,
                    path_param,
                    query: query.to_string(),
                    body: body.to_string(),
                    dispatch: module.dispatch,
                    reply,
                });
            }
        }
        None
    }

    /// Starts the accept loop on its own OS thread when `self.port` can be
    /// bound, and answers `None` -- no thread started -- for every other
    /// case (port already in use). A caller that gets `None` back runs
    /// with no agentic surface at all, exactly as if this crate were not
    /// linked in.
    ///
    /// **What this method does not do, and why it stays the caller's
    /// job**: draining the returned channel and forcing a repaint on a
    /// backgrounded window. `debug_http.rs`'s own module doc comment
    /// documents the measured bug this fixes (130 `cx.notify()` calls
    /// over 8 seconds produced 2 renders on a backgrounded window) and the
    /// fix (a dedicated `cx.spawn` loop calling `cx.update_window` every
    /// 100ms) -- that fix is written in terms of `Window`/`Entity<Self>`
    /// types this crate has no dependency on `gpui` to name, so every
    /// consumer wires their own copy of that loop around the channel this
    /// method returns, the same shape that app's root entity already has.
    ///
    /// **Borrow-checker note**: the accept-loop closure below must be
    /// `'static` (it runs on its own `std::thread::spawn`'d thread), so it
    /// cannot hold a `&self` borrow -- `self` is a local that does not
    /// outlive this method call. The fix is to move `self.modules` (an
    /// owned `Vec<ModuleRegistration<T>>`) into the closure before
    /// spawning, and route against that owned `Vec` via
    /// [`AgenticServer::route_over`] (a free-standing associated function
    /// over `&[ModuleRegistration<T>]`) rather than calling `self.route`
    /// from inside the thread.
    pub fn spawn(self) -> Option<std::sync::mpsc::Receiver<AgenticCall<T>>> {
        let server = match tiny_http::Server::http(("127.0.0.1", self.port)) {
            Ok(server) => server,
            Err(e) => {
                eprintln!(
                    "gpui-agentic-http: could not bind 127.0.0.1:{}: {e}",
                    self.port
                );
                return None;
            }
        };
        let (calls, receiver) = std::sync::mpsc::channel();
        let app_name = self.app_name;
        let capabilities = self.capabilities_json();
        let modules = self.modules;
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let method = request.method().as_str().to_string();
                let full_path = request.url().to_string();
                let (path, query) = full_path
                    .split_once('?')
                    .unwrap_or((full_path.as_str(), ""));
                let mut request_body = String::new();
                let _ = request.as_reader().read_to_string(&mut request_body);

                if method == "GET" && path == "/" {
                    let response = tiny_http::Response::from_string(capabilities.to_string())
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .expect("static header name and value are always valid"),
                        );
                    let _ = request.respond(response);
                    continue;
                }

                let Some(call) = Self::route_over(&modules, &method, path, query, &request_body)
                else {
                    let _ = request.respond(
                        tiny_http::Response::from_string("not found").with_status_code(404),
                    );
                    continue;
                };
                let (reply, answer) = std::sync::mpsc::channel();
                let call = AgenticCall { reply, ..call };
                if calls.send(call).is_err() {
                    let _ = request.respond(
                        tiny_http::Response::from_string(format!("{app_name} is shutting down"))
                            .with_status_code(503),
                    );
                    continue;
                }
                let body = answer
                    .recv()
                    .unwrap_or_else(|_| serde_json::json!({ "error": "no reply" }));
                let response = tiny_http::Response::from_string(body.to_string()).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("static header name and value are always valid"),
                );
                let _ = request.respond(response);
            }
        });
        Some(receiver)
    }
}

/// A `?name=value&name2=value2` query string to its decoded pairs --
/// **new functionality, not connective tissue**: neither of the two
/// apps it was extracted from had a compliant
/// parameterized `GET` before this crate existed (see the design spec's
/// own "What ships with the crate" section), so nothing already exercises
/// this. No external crate: the format is simple enough that pulling in
/// `url`/`form_urlencoded` for it would be a dependency for four lines of
/// logic.
pub fn parse_query_string(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((percent_decode(name), percent_decode(value)))
        })
        .collect()
}

/// The one escaping query strings actually need for this crate's own
/// params (plain identifiers, numbers, short search terms) -- `%XX` and
/// `+` for space, the minimal convention of a urlencode helper. Not a
/// general-purpose decoder; a malformed `%` sequence is passed through
/// literally rather than erroring, the same "best effort, never panics on
/// untrusted input" stance `debug_http.rs`'s own body parsing already
/// takes (`serde_json::from_str(body).ok()?`, never `.unwrap()`).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(' '),
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) => out.push(byte as char),
                    Err(_) => {
                        out.push('%');
                        out.push_str(&hex);
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod query_string_tests {
    use super::*;

    #[test]
    fn empty_query_is_no_pairs() {
        assert_eq!(parse_query_string(""), vec![]);
    }

    #[test]
    fn one_pair_is_decoded() {
        assert_eq!(
            parse_query_string("simulation_id=42"),
            vec![("simulation_id".to_string(), "42".to_string())]
        );
    }

    #[test]
    fn two_pairs_are_both_decoded() {
        assert_eq!(
            parse_query_string("simulation_id=42&state=assigned"),
            vec![
                ("simulation_id".to_string(), "42".to_string()),
                ("state".to_string(), "assigned".to_string()),
            ]
        );
    }

    #[test]
    fn percent_and_plus_encoding_decode() {
        assert_eq!(
            parse_query_string("q=hello%20world%21&plus=a+b"),
            vec![
                ("q".to_string(), "hello world!".to_string()),
                ("plus".to_string(), "a b".to_string()),
            ]
        );
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::registry::{Action, ModuleRegistration, Param, ParamLocation};

    #[derive(Default)]
    struct FakeApp;

    fn dispatch(
        _app: &mut FakeApp,
        name: &str,
        _p: Option<&str>,
        _q: &str,
        _b: &str,
    ) -> serde_json::Value {
        serde_json::json!({"called": name})
    }

    const ACTIONS: &[Action] = &[
        Action {
            name: "state",
            method: "GET",
            path: "/state",
            description: "d",
            params: &[],
        },
        Action {
            name: "select_vehicle",
            method: "POST",
            path: "/select-vehicle/{id}",
            description: "d",
            params: &[Param {
                name: "id",
                kind: "integer",
                location: ParamLocation::Path,
                description: "d",
            }],
        },
    ];

    fn server() -> AgenticServer<FakeApp> {
        AgenticServer::new("fake-app", 0).register(ModuleRegistration {
            prefix: "fleet",
            actions: ACTIONS,
            dispatch,
        })
    }

    #[test]
    fn a_prefixed_flat_action_routes() {
        let call = server()
            .route("GET", "/fleet/state", "", "")
            .expect("must route");
        assert_eq!(call.action_name, "state");
    }

    #[test]
    fn a_prefixed_path_param_action_routes_and_carries_the_param() {
        let call = server()
            .route("POST", "/fleet/select-vehicle/42", "", "")
            .expect("must route");
        assert_eq!(call.action_name, "select_vehicle");
        assert_eq!(call.path_param.as_deref(), Some("42"));
    }

    #[test]
    fn the_wrong_prefix_does_not_route() {
        assert!(server().route("GET", "/other/state", "", "").is_none());
    }

    #[test]
    fn the_wrong_method_does_not_route() {
        assert!(server().route("POST", "/fleet/state", "", "").is_none());
    }

    #[test]
    fn an_unknown_action_under_a_real_prefix_does_not_route() {
        assert!(server().route("GET", "/fleet/unknown", "", "").is_none());
    }
}
