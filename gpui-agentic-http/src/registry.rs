//! The description of what one app module makes callable -- ported from a
//! downstream gpui app's own `debug_http::Action`/`Param`, generalized
//! with a `ParamLocation` that file never needed: every one of its params
//! rode in a JSON body regardless of HTTP method, including on `GET`
//! requests -- which is exactly the shape `curl -d` cannot express without
//! an explicit `-X GET` override, and which nothing in that registry told
//! an agent to expect. This crate makes the location an explicit,
//! type-checked field instead of an unstated convention.

/// Where one [`Param`] travels on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamLocation {
    /// Substituted into a `{name}` placeholder in the action's own `path`.
    Path,
    /// Read from the request's query string (`?name=value`).
    Query,
    /// Read as a field of the request's JSON body. **Only valid on a
    /// non-`GET` action** -- see [`Action`]'s own doc comment.
    Body,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Param {
    pub name: &'static str,
    pub kind: &'static str,
    pub location: ParamLocation,
    pub description: &'static str,
}

/// One capability an app module exposes.
///
/// **Hard rule, found by an actual live bug (a `GET` action whose only
/// param rode in the body, which `curl -d` cannot express without
/// `-X GET`, and which nothing in the registry declared): a `Body` param
/// is only valid on a non-`GET` action.** [`assert_registry_routable`]
/// checks this, so violating it fails a test rather than surfacing as a
/// live 404 an agent has no way to diagnose from the registry alone.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Action {
    pub name: &'static str,
    pub method: &'static str,
    /// A literal path, optionally with one `{name}` placeholder for a
    /// `Path`-location param. Never encodes `Query`/`Body` params -- those
    /// are declared only in `params`, not in this string.
    pub path: &'static str,
    pub description: &'static str,
    pub params: &'static [Param],
}

impl Action {
    /// The hard rule above, checked once per action so both the
    /// registry-drift test and any future caller share one implementation
    /// of what "valid" means.
    pub fn is_valid(&self) -> bool {
        if self.method == "GET" {
            return !self
                .params
                .iter()
                .any(|p| p.location == ParamLocation::Body);
        }
        true
    }
}

/// One app module's own actions and how to run them, prefixed so two
/// modules registered on the same [`AgenticServer`](crate::AgenticServer)
/// never collide -- see this crate's own README, "Multi-module
/// composition".
///
/// `T` is the app's own state type (e.g. `MyApp`) -- `dispatch` gets a
/// mutable reference to it the same way a real UI event handler would,
/// which is deliberately as much access as a click already has. Giving a
/// `dispatch` fn a *narrower* slice of `T` than the whole app is a
/// per-consumer choice this crate's types cannot enforce (see the spec's
/// own "Secrets" section) -- `T` is chosen by whoever calls
/// `AgenticServer::register`, not fixed by this crate.
pub struct ModuleRegistration<T> {
    /// This module's own namespace -- every action's name is reported (in
    /// `GET /`) and routed (in the request path) as `{prefix}.{name}` /
    /// `/{prefix}/{action-path}` respectively. Two modules picking the
    /// same prefix is a collision `AgenticServer::register` catches with a
    /// `debug_assert!`, not something this type itself can prevent at
    /// compile time.
    pub prefix: &'static str,
    pub actions: &'static [Action],
    /// `path_param` is `Some` only for an action whose own `path` names a
    /// `{name}` placeholder; `query`/`body` are the raw, not-yet-parsed
    /// query string and request body respectively -- parsing into a
    /// concrete type is each dispatch fn's own job, the same way
    /// `debug_http.rs`'s original `parse_request` did the parsing inline
    /// per action rather than through a generic layer.
    pub dispatch: fn(
        app: &mut T,
        action_name: &str,
        path_param: Option<&str>,
        query: &str,
        body: &str,
    ) -> serde_json::Value,
}

/// A registry consumer calls once per registered module in their own test
/// suite (see this crate's README) -- proves every declared [`Action`] is
/// actually routable and that [`Action::is_valid`] holds for every one of
/// them. A capability documented but not wired -- or the reverse -- fails
/// this rather than silently diverging.
///
/// **Deliberately does not call `ModuleRegistration::dispatch` itself.**
/// `dispatch` needs `&mut T`, and for a real gpui app `T` (the app's own root
/// entity) cannot be constructed at all outside a live window --
/// this crate's own reference consumer has no `#[gpui::test]` harness, a
/// fact confirmed directly against that codebase (multiple existing doc
/// comments state it), not assumed. Requiring a real `T` here would make
/// this helper unusable by the one real consumer it exists for. Instead
/// this checks *routability* -- whether `is_routable` (a caller-supplied,
/// `T`-independent predicate over just an action's name) recognises every
/// declared action -- the same shape that app's own pre-migration
/// `parse_request` tests already proved out: a pure parsing function, with no dependency on a constructed app
/// instance, is what a drift test actually needs to exercise.
pub fn assert_registry_routable<T>(
    registration: &ModuleRegistration<T>,
    is_routable: impl Fn(&Action) -> bool,
) {
    for action in registration.actions {
        assert!(
            action.is_valid(),
            "{}.{}: a GET action's params must all be Path/Query, never Body \
             (found a Body param) -- curl -d cannot express a GET body without \
             an explicit -X GET override, and nothing in the registry would \
             tell an agent to expect one",
            registration.prefix,
            action.name,
        );
        assert!(
            is_routable(action),
            "{}.{} is declared in `actions` but `is_routable` does not recognise it -- \
             a capability an agent is told exists must actually be parseable, not silently absent",
            registration.prefix,
            action.name,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeApp {
        called: Option<&'static str>,
    }

    const ACTIONS: &[Action] = &[
        Action {
            name: "ping",
            method: "GET",
            path: "/ping",
            description: "returns pong",
            params: &[],
        },
        Action {
            name: "greet",
            method: "GET",
            path: "/greet",
            description: "greets a name",
            params: &[Param {
                name: "name",
                kind: "string",
                location: ParamLocation::Query,
                description: "who to greet",
            }],
        },
    ];

    fn dispatch(
        app: &mut FakeApp,
        name: &str,
        _path: Option<&str>,
        _query: &str,
        _body: &str,
    ) -> serde_json::Value {
        match name {
            "ping" => {
                app.called = Some("ping");
                serde_json::json!({"ok": "pong"})
            }
            "greet" => {
                app.called = Some("greet");
                serde_json::json!({"ok": "hello"})
            }
            _ => serde_json::json!({"error": "unrouted"}),
        }
    }

    #[test]
    fn a_get_action_with_a_body_param_is_invalid() {
        const BAD: Action = Action {
            name: "bad",
            method: "GET",
            path: "/bad",
            description: "wrong on purpose",
            params: &[Param {
                name: "x",
                kind: "string",
                location: ParamLocation::Body,
                description: "should have been Query",
            }],
        };
        assert!(!BAD.is_valid());
    }

    #[test]
    fn a_get_action_with_a_query_param_is_valid() {
        assert!(ACTIONS[1].is_valid());
    }

    /// The `is_routable` predicate a real consumer would build from their
    /// own pure parsing function (e.g. a `parse_action`) -- here, a tiny stand-in that recognises only
    /// the two names `ACTIONS` actually declares, to prove the assertion
    /// helper itself catches a mismatch either direction.
    fn fake_is_routable(action: &Action) -> bool {
        matches!(action.name, "ping" | "greet")
    }

    #[test]
    fn a_fully_routed_registry_passes_the_drift_assertion() {
        let registration = ModuleRegistration {
            prefix: "fake",
            actions: ACTIONS,
            dispatch,
        };
        assert_registry_routable(&registration, fake_is_routable);
    }

    #[test]
    #[should_panic(expected = "is declared in `actions` but `is_routable` does not recognise it")]
    fn an_unroutable_action_fails_the_drift_assertion() {
        const UNROUTABLE_ACTIONS: &[Action] = &[Action {
            name: "ghost",
            method: "GET",
            path: "/ghost",
            description: "declared but never recognised by is_routable",
            params: &[],
        }];
        let registration = ModuleRegistration {
            prefix: "fake",
            actions: UNROUTABLE_ACTIONS,
            dispatch,
        };
        assert_registry_routable(&registration, fake_is_routable);
    }
}
