//! Source-scan helpers shared by the parity gates.
//!
//! `camera_is_written_by_the_controller`, `one_cover_for_every_shell` and
//! `space_plan_parity` all read a shell's source text and look for a banned
//! pattern in the CODE, not in a comment or a string literal. Before this
//! file each gate re-derived its own `repo_root` and `read`; this is the one
//! place per Ruling F16(d).
//!
//! `tests/common/mod.rs` (a subdirectory's `mod.rs`), not `tests/common.rs`,
//! is what keeps this from becoming a fourth or fifth test BINARY: cargo
//! only turns a direct child of `tests/` into its own test target, so a
//! `mod common;` in a gate file compiles this as an ordinary module of that
//! gate's binary instead. Not every gate uses every helper here -- the
//! module is `#[allow(dead_code)]` throughout so that omission doesn't warn
//! once per binary that doesn't need one.
//!
//! Fix round 1: `code_on` (per line) and `without_strings` (a stateful
//! toggle with no memory between lines) are retired in favour of
//! `code_only`, a single whole-file pass. Two real problems drove this:
//! `code_on`-only matchers (`banned_calls`, `other_flat_cover_calls`) never
//! stripped strings at all, so a banned name inside an assertion message
//! would false-fail; and `without_strings`' per-line, boolean `in_string`
//! toggle cannot survive a multi-line string, doesn't know a raw string's
//! quotes don't need escaping, and -- worse -- a single stray `'"'` char
//! literal flips its toggle for the rest of the scan, which can HIDE a real
//! violation rather than merely miss a fake one.
//!
//! Fix round 2: an unterminated string, raw string or block comment used to
//! blank everything from its opening delimiter to EOF with no complaint --
//! the closing-search loops simply `break` on `p >= n`. That is unreachable
//! in code that compiles, but it means a gate that hits one goes quietly
//! blind over everything after it rather than failing loudly, which is
//! exactly backwards for a gate whose entire job is to fail loudly. All
//! three now `panic!` (with the opening delimiter's line and byte offset)
//! instead of blanking to EOF.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The repo root, from Cargo rather than a hardcoded path -- correct
/// regardless of the directory `cargo test` happens to run from.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// The 1-based line number and the byte offset (not char index -- what a
/// human reading the raw file would recognise) of `chars[char_index]`,
/// counted from the start of the file. Only ever called on a `code_only`
/// panic path, so an O(n) recount here costs nothing on the hot path.
fn line_and_byte_offset(chars: &[char], char_index: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut byte_offset = 0usize;
    for &c in &chars[..char_index.min(chars.len())] {
        byte_offset += c.len_utf8();
        if c == '\n' {
            line += 1;
        }
    }
    (line, byte_offset)
}

/// `src` with every comment, string literal and char literal blanked to
/// spaces, one whole file at a time rather than one line at a time -- a
/// block comment or a string can itself contain a newline, which a
/// per-line version cannot see past. Newlines themselves are always kept,
/// so `code_only(src).lines()` lines up one for one with `src.lines()`.
///
/// Blanks:
/// - line comments (`//`, `///`, `//!` -- all the same token to the lexer,
///   so no special case is needed for the doc-comment spellings), to the
///   end of the line;
/// - block comments (`/* */`, `/** */`, `/*! */`), nested to any depth;
/// - string literals: normal (`"..."`), byte (`b"..."`), raw (`r"..."`,
///   `r#"..."#`, any number of `#`) and raw byte (`br"..."`, `br#"..."#`);
///   any of these may legally contain a literal, un-escaped newline (a
///   multi-line string), which stays a newline in the output;
/// - char literals (`'a'`, `'"'`, `'\''`, `'\\'`, `'\n'`, `'\x41'`,
///   `'\u{1F600}'`).
///
/// Does NOT blank a lifetime (`'a`, `'static`, `'_`). A `'` that starts an
/// escape, or is followed by exactly one plain character and then a
/// closing `'`, is a char literal; anything else is a lifetime, told apart
/// the same way rustc's own lexer tells them apart, and its letters are
/// left as ordinary code -- unlike a stateful toggle, which cannot tell a
/// lifetime's `'` from the start of a string and would desynchronise for
/// everything after it.
pub fn code_only(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // Line comment: `//`, `///`, `//!` alike. Runs to (not including)
        // the newline, which the default branch below then copies as-is.
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut p = i + 2;
            while p < n && chars[p] != '\n' {
                p += 1;
            }
            for _ in i..p {
                out.push(' ');
            }
            i = p;
            continue;
        }

        // Block comment: `/* ... */`, `/** ... */`, `/*! ... */` alike,
        // nested to any depth.
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let start = i;
            let mut p = i + 2;
            let mut depth = 1u32;
            while p < n && depth > 0 {
                if chars[p] == '/' && p + 1 < n && chars[p + 1] == '*' {
                    depth += 1;
                    p += 2;
                } else if chars[p] == '*' && p + 1 < n && chars[p + 1] == '/' {
                    depth -= 1;
                    p += 2;
                } else {
                    p += 1;
                }
            }
            if depth > 0 {
                let (line, byte_offset) = line_and_byte_offset(&chars, start);
                panic!(
                    "code_only: unterminated block comment opened at line {line} (byte offset \
                     {byte_offset}) -- reached end of file with {depth} level(s) still open. \
                     Blanking to EOF here would silently hide every real violation after it."
                );
            }
            for &c in &chars[start..p] {
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            i = p;
            continue;
        }

        // A raw (optionally byte) string: an optional `b`, then `r`, then
        // any number of `#`, then the opening `"`. Anything starting with
        // `b` or `r` that isn't actually followed by this shape (a plain
        // identifier such as `raw` or `bar`) falls through untouched.
        {
            let mut cursor = i;
            if chars[cursor] == 'b' && cursor + 1 < n && chars[cursor + 1] == 'r' {
                cursor += 1;
            }
            if cursor < n && chars[cursor] == 'r' {
                let mut k = cursor + 1;
                let mut hashes = 0usize;
                while k < n && chars[k] == '#' {
                    hashes += 1;
                    k += 1;
                }
                if k < n && chars[k] == '"' {
                    let mut p = k + 1;
                    let mut closed = false;
                    while p < n {
                        if chars[p] == '"' {
                            let mut q = p + 1;
                            let mut h = 0usize;
                            while h < hashes && q < n && chars[q] == '#' {
                                q += 1;
                                h += 1;
                            }
                            if h == hashes {
                                p = q;
                                closed = true;
                                break;
                            }
                        }
                        p += 1;
                    }
                    if !closed {
                        let (line, byte_offset) = line_and_byte_offset(&chars, i);
                        panic!(
                            "code_only: unterminated raw string literal opened at line {line} \
                             (byte offset {byte_offset}) -- reached end of file looking for its \
                             closing `\"{}`. Blanking to EOF here would silently hide every real \
                             violation after it.",
                            "#".repeat(hashes)
                        );
                    }
                    for &c in &chars[i..p] {
                        out.push(if c == '\n' { '\n' } else { ' ' });
                    }
                    i = p;
                    continue;
                }
            }
        }

        // A normal or byte string (not raw): an optional `b`, then `"`,
        // running to the matching un-escaped `"`. May contain a literal
        // newline (a multi-line string), which is kept in the output.
        {
            let mut cursor = i;
            if chars[cursor] == 'b' && cursor + 1 < n && chars[cursor + 1] == '"' {
                cursor += 1;
            }
            if cursor < n && chars[cursor] == '"' {
                let mut p = cursor + 1;
                let mut escaped = false;
                let mut closed = false;
                while p < n {
                    if escaped {
                        escaped = false;
                        p += 1;
                        continue;
                    }
                    match chars[p] {
                        '\\' => {
                            escaped = true;
                            p += 1;
                        }
                        '"' => {
                            p += 1;
                            closed = true;
                            break;
                        }
                        _ => p += 1,
                    }
                }
                if !closed {
                    let (line, byte_offset) = line_and_byte_offset(&chars, i);
                    panic!(
                        "code_only: unterminated string literal opened at line {line} (byte \
                         offset {byte_offset}) -- reached end of file still inside it. Blanking \
                         to EOF here would silently hide every real violation after it."
                    );
                }
                for &c in &chars[i..p] {
                    out.push(if c == '\n' { '\n' } else { ' ' });
                }
                i = p;
                continue;
            }
        }

        // A char literal, told apart from a lifetime the way rustc tells
        // them apart: an escape, or exactly one plain character, closed by
        // a `'`, is a char literal; anything else is a lifetime and is
        // left as ordinary code below.
        if c == '\'' {
            if i + 1 < n && chars[i + 1] == '\\' {
                let mut p = i + 2;
                if p < n {
                    match chars[p] {
                        'x' => {
                            p += 1;
                            let mut k = 0;
                            while k < 2 && p < n && chars[p].is_ascii_hexdigit() {
                                p += 1;
                                k += 1;
                            }
                        }
                        'u' => {
                            p += 1;
                            if p < n && chars[p] == '{' {
                                p += 1;
                                while p < n && chars[p] != '}' {
                                    p += 1;
                                }
                                if p < n {
                                    p += 1;
                                }
                            }
                        }
                        _ => p += 1,
                    }
                }
                if p < n && chars[p] == '\'' {
                    p += 1;
                    for _ in i..p {
                        out.push(' ');
                    }
                    i = p;
                    continue;
                }
                // Not actually closed where expected; treat the opening
                // `'` as ordinary code below rather than guess further.
            } else if i + 2 < n && chars[i + 1] != '\'' && chars[i + 2] == '\'' {
                out.push(' ');
                out.push(' ');
                out.push(' ');
                i += 3;
                continue;
            }
            // Otherwise a lifetime (or an unrecognised edge case) -- fall
            // through and leave the `'` as ordinary code.
        }

        out.push(c);
        i += 1;
    }

    out
}

/// Every `.rs` file under `dir`, recursively, skipping any `target` directory.
pub fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
