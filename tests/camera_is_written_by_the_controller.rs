//! No shell writes the camera's orientation, zoom or centre itself.
//!
//! The three shells used to own three copies of the camera's rules, and they
//! disagreed: native wrapped bearing into [-PI, PI) and snapped north on
//! release, iOS wrapped into [0, 2*PI) and snapped live, and the browser
//! could not rotate at all (see
//! `docs/superpowers/specs/2026-09-24-3d-camera-parity-design.md`). All
//! three now drive `src/camera_control.rs`. This file stops a fourth copy
//! from growing back.
//!
//! As blunt as `tests/space_plan_parity.rs`'s
//! `no_shell_calls_a_space_gate_directly`, for the same reason: a scan that
//! has to reason about context can be quietly defeated by a later edit. It
//! reads the WHOLE file, tests included -- unlike `space_plan_parity.rs`,
//! which cuts its scan off before `src/map_view.rs`'s `mod tests` because
//! that module holds a DELIBERATE direct call the production code must
//! never make. There is no such carve-out needed here: tests reach the
//! camera through `CameraController::camera_mut()`, and `camera_mut().pitch
//! = x` does not contain the substring `camera.pitch` (the `_mut()` sits
//! between them), so the one legitimate test-only path is naturally
//! invisible to this scan without excluding a single line of test code --
//! and a PRODUCTION write slipped into a test module, e.g. `self.camera.zoom
//! = 2.0` inside `#[cfg(test)] mod tests`, still matches and still fails the
//! gate.

mod common;

use common::{code_only, read, repo_root};

const SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];
const FIELDS: [&str; 5] = ["pitch", "bearing", "zoom", "center_lon", "center_lat"];
const BANNED_CALLS: [&str; 2] = ["wrap_bearing(", "normalize_bearing("];

/// Every assignment to `camera.<field>` in `source`'s CODE, as
/// `"<line>: <text>"` (the text is the ORIGINAL line, not the stripped one,
/// so the report reads like the file). `code_only` blanks every comment and
/// string/char literal first, so a mention inside either -- an assertion
/// message's `camera.zoom={}`, a doc comment naming the field -- is never
/// read as an assignment. An assignment is the WHOLE field name, on both
/// sides (`flat_camera.pitch` does not count; neither does
/// `camera.pitchfork`), followed (after spaces) by `=` -- not `==` or `=>`
/// -- or by a compound `+=`, `-=`, `*=` or `/=`.
fn camera_field_writes(source: &str) -> Vec<String> {
    let stripped = code_only(source);
    let mut out = Vec::new();
    for (number, (code, original)) in stripped.lines().zip(source.lines()).enumerate() {
        for field in FIELDS {
            let needle = format!("camera.{field}");
            let mut from = 0;
            while let Some(at) = code[from..].find(&needle) {
                let start = from + at;
                let end = start + needle.len();
                let boundary_before = code[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                let after = &code[end..];
                let whole_name = !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
                let rest = after.trim_start();
                let plain = rest.starts_with('=') && !rest.starts_with("==") && !rest.starts_with("=>");
                let compound = ["+=", "-=", "*=", "/="].iter().any(|op| rest.starts_with(op));
                if boundary_before && whole_name && (plain || compound) {
                    out.push(format!("{}: {}", number + 1, original.trim()));
                }
                from = end;
            }
        }
    }
    out
}

/// Calls to `wrap_bearing(`/`normalize_bearing(` in `source`'s CODE (same
/// stripping as above, so a mention inside a comment or a string -- e.g. an
/// assertion message quoting the name -- is never read as a call). Requires
/// a word boundary before the match, the same way `other_flat_cover_calls`
/// does for `flat_cover_`, so a differently-named function such as
/// `custom_wrap_bearing(` does not count.
fn banned_calls(source: &str) -> Vec<String> {
    let stripped = code_only(source);
    let mut out = Vec::new();
    for (number, (code, original)) in stripped.lines().zip(source.lines()).enumerate() {
        let mut matched = false;
        for call in BANNED_CALLS {
            let mut from = 0;
            while let Some(at) = code[from..].find(call) {
                let start = from + at;
                let boundary_before = code[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                if boundary_before {
                    matched = true;
                }
                from = start + call.len();
            }
        }
        if matched {
            out.push(format!("{}: {}", number + 1, original.trim()));
        }
    }
    out
}

#[test]
fn the_scanner_finds_what_it_is_for() {
    let planted = "self.camera.pitch = 1.0;\n\
                   a.camera.bearing += delta;\n\
                   c.camera.center_lat=4.0;\n\
                   if camera.zoom == 3.0 {}\n\
                   let z = camera.zoom;\n\
                   // camera.zoom = 1.0;\n\
                   camera.zoom_level = 2;\n\
                   match x { _ => camera.pitch }\n\
                   assert!(ok, \"mid-ease; camera.zoom={} target={}\", z, t);\n\
                   let b = wrap_bearing(x);\n\
                   let flat_camera.pitch = 9.0;\n\
                   let c = custom_wrap_bearing(x);\n";
    // The second-to-last planted line is fix round 1's MINOR:
    // `flat_camera.pitch` contains the needle `camera.pitch` as a substring
    // of a DIFFERENT name, and must not count -- only a boundary check on
    // both sides of the match (not just after it) catches this. The last
    // line is fix round 2's MINOR: `custom_wrap_bearing(` contains
    // `wrap_bearing(` as a substring of a DIFFERENT function's name, and
    // must not count either -- same fix, applied to `banned_calls`.
    assert_eq!(camera_field_writes(planted).len(), 3, "{:?}", camera_field_writes(planted));
    assert_eq!(banned_calls(planted).len(), 1, "{:?}", banned_calls(planted));
}

/// `code_only` (via the two matchers above, its real consumers) on the five
/// cases fix round 1 called out: a `'"'` char literal must not swallow the
/// rest of its line, a raw string and a multi-line string must both hide
/// their contents from the scan, a lifetime must not be mistaken for a char
/// literal (which would hide everything after it), and a nested block
/// comment must be stripped in full.
#[test]
fn code_only_strips_comments_strings_and_char_literals_without_hiding_lifetimes() {
    // A `'"'` char literal followed by a real violation on the SAME line:
    // the violation must still be caught. A stateful, line-oblivious
    // quote-toggle (the retired `without_strings`) would instead flip
    // "inside a string" at the `'"'` and swallow everything after it.
    let same_line = "let quote = '\"'; camera.pitch = 1.0;";
    assert_eq!(camera_field_writes(same_line).len(), 1, "{:?}", camera_field_writes(same_line));

    // A raw string containing the violating text verbatim must NOT be
    // caught -- it is data, not code, and raw strings need no escaping.
    let raw = "let s = r#\"camera.pitch = 1.0\"#;";
    assert!(camera_field_writes(raw).is_empty(), "{:?}", camera_field_writes(raw));

    // A multi-line (normal) string containing a banned call must NOT be
    // caught, and must not desynchronise the line count either -- proven
    // by feeding it through `banned_calls`, which zips stripped lines
    // against the original ones.
    let multiline = "let s = \"before\nwrap_bearing(\nafter\";";
    assert!(banned_calls(multiline).is_empty(), "{:?}", banned_calls(multiline));

    // A lifetime ahead of a real violation on the same line must NOT hide
    // it: `'a` is not a char literal (it isn't closed by a `'` right after
    // one character), so it must be left as ordinary code.
    let lifetime = "fn f<'a>(c: &'a mut X) { camera.bearing = 2.0; }";
    assert_eq!(camera_field_writes(lifetime).len(), 1, "{:?}", camera_field_writes(lifetime));

    // A nested block comment is stripped in full, banned call inside
    // included.
    let nested = "/* outer /* wrap_bearing( inner */ still comment */ let ok = 1;";
    assert!(banned_calls(nested).is_empty(), "{:?}", banned_calls(nested));
}

/// Fix round 2: an unterminated string or raw string used to blank
/// everything from its opening delimiter to EOF with no complaint --
/// exactly what the re-reviewer showed hides two real violations at once
/// (`"let s = \"never closes\nwrap_bearing(x);\ncamera.pitch = 1.0;"`).
/// `code_only` must now panic instead of silently going blind.
#[test]
#[should_panic(expected = "unterminated string")]
fn code_only_panics_on_an_unterminated_string() {
    camera_field_writes("let s = \"never closes\nwrap_bearing(x);\ncamera.pitch = 1.0;");
}

#[test]
#[should_panic(expected = "unterminated raw string")]
fn code_only_panics_on_an_unterminated_raw_string() {
    camera_field_writes("let s = r#\"never closes\ncamera.pitch = 1.0;");
}

/// The third unterminated construct fix round 1 called out (not asked for
/// as a test, but cheap to prove alongside the other two): a block comment
/// that never closes must panic the same way, rather than blank to EOF.
#[test]
#[should_panic(expected = "unterminated block comment")]
fn code_only_panics_on_an_unterminated_block_comment() {
    camera_field_writes("/* outer /* nested, never closes\ncamera.pitch = 1.0;");
}

#[test]
fn camera_is_written_by_the_controller() {
    for shell in SHELLS {
        let source = read(&repo_root().join(shell));
        assert!(
            source.len() > 2_000,
            "{shell} read as only {} bytes -- this gate would be checking nothing",
            source.len()
        );
        let writes = camera_field_writes(&source);
        assert!(
            writes.is_empty(),
            "{shell} writes camera fields itself. Every change to pitch, bearing, zoom or centre goes \
             through `camera_control::CameraController` (`apply`, `jump_to`, `ease_to`) -- a shell that \
             writes one directly has re-grown its own copy of the camera's rules:\n  {}",
            writes.join("\n  ")
        );
        let calls = banned_calls(&source);
        assert!(
            calls.is_empty(),
            "{shell} wraps a bearing itself; bearing wrapping is `camera_control`'s alone:\n  {}",
            calls.join("\n  ")
        );
        // The zero-writes assertion is also satisfied by a shell that stopped
        // moving the camera at all. Requiring the controller in use tells the
        // two apart.
        assert!(
            source.contains("camera_control.apply("),
            "{shell} never calls `camera_control.apply(` -- it may have dropped camera input entirely"
        );
    }
}
