//! The caption track: SRT, VTT, and the burn-in default (spec §5).
//!
//! [`Timeline::caption_inputs`](super::timeline::Timeline::caption_inputs)
//! gives the raw `(from, to, text)` triples -- a resolved `caption` event
//! first, then `script.captions`, in time order. [`track`] turns them into
//! the one, final [`Cue`] list every export reads: [`srt`], [`vtt`] and
//! (Task 16) the transcript. [`burn_in_default`] is the player's separate
//! question -- whether the SAME text is also drawn into the frame.
//!
//! Pure and deterministic: no clock, no I/O, wasm/iOS-safe (this file is
//! `#[path]`-included by `web/src/lib.rs` and `ios/src/lib.rs`).
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use super::tiers::Aspect;

/// One cue of the final caption track, ready to write: `index` is 1-based
/// (the SRT cue number), `from`/`to` are story seconds rounded to whole
/// milliseconds, and `text` is the raw, unescaped caption.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Cue {
    pub index: usize,
    pub from: f64,
    pub to: f64,
    pub text: String,
}

/// `inputs` (as [`Timeline::caption_inputs`](super::timeline::Timeline::caption_inputs)
/// gives them) folded into the final track: clamped to `[0, duration]`,
/// rounded to whole milliseconds, with any cue that is then empty-text or
/// zero-length dropped, sorted by `from` (a stable sort, so cues already at
/// the same `from` keep their input order). Overlapping cues are kept --
/// both SRT and VTT allow them, and nothing here merges or reorders them
/// past that.
#[must_use]
pub fn track(inputs: &[(f64, f64, String)], duration: f64) -> Vec<Cue> {
    let mut cues: Vec<(f64, f64, String)> = inputs
        .iter()
        .map(|(from, to, text)| (round_ms(from.clamp(0.0, duration)), round_ms(to.clamp(0.0, duration)), text.clone()))
        .filter(|(from, to, text)| to > from && !text.trim().is_empty())
        .collect();
    cues.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    cues.into_iter().enumerate().map(|(i, (from, to, text))| Cue { index: i + 1, from, to, text }).collect()
}

/// `seconds` rounded to the nearest whole millisecond (half away from
/// zero, as `f64::round` does), still in seconds.
fn round_ms(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

/// `seconds` (already millisecond-rounded) as whole milliseconds,
/// never negative.
fn whole_ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round().max(0.0) as i64
}

/// `total_ms` split into hours, minutes, seconds and milliseconds.
fn split_ms(total_ms: i64) -> (i64, i64, i64, i64) {
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    (h, m, s, ms)
}

/// `HH:MM:SS,mmm` (SRT).
fn srt_timestamp(seconds: f64) -> String {
    let (h, m, s, ms) = split_ms(whole_ms(seconds));
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

/// `HH:MM:SS.mmm` (VTT).
fn vtt_timestamp(seconds: f64) -> String {
    let (h, m, s, ms) = split_ms(whole_ms(seconds));
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// SubRip has no entity escaping of its own, and a literal `-->` in the
/// text would read exactly like a timing line to a line-structured parser
/// (including the one in this module's own tests). The controller's
/// ruling: write it as a plain `->` instead of an entity -- a real SRT
/// player shows exactly what is in the file, never decodes `&gt;` -- so
/// this is a lossy, one-way rewrite (an original `->` and an original
/// `-->` both read back as `->`), unlike VTT's reversible entity escaping.
fn escape_srt(text: &str) -> String {
    text.replace("-->", "->")
}

/// WebVTT cue-payload escaping: `&` first (so the entities this introduces
/// are not themselves re-escaped), then `<`, then `-->` to `--&gt;` (SRT
/// instead writes a plain `->`, per `escape_srt` -- VTT has real entity
/// escaping, so it keeps `-->` reversible instead of discarding it).
/// `>` alone is left alone -- the spec names only `-->`, `<` and `&`.
fn escape_vtt(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace("-->", "--&gt;")
}

/// SubRip: `"1\n00:00:01,000 --> 00:00:03,000\ntext\n\n"` per cue, in
/// order; an empty track is the empty string. Newlines inside a cue's text
/// are kept as given.
#[must_use]
pub fn srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for cue in cues {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", cue.index, srt_timestamp(cue.from), srt_timestamp(cue.to), escape_srt(&cue.text)));
    }
    out
}

/// WebVTT: `"WEBVTT\n\n"` then `"00:00:01.000 --> 00:00:03.000\ntext\n\n"`
/// per cue. An empty track is exactly `"WEBVTT\n\n"` (Review Focus 5).
#[must_use]
pub fn vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format!("{} --> {}\n{}\n\n", vtt_timestamp(cue.from), vtt_timestamp(cue.to), escape_vtt(&cue.text)));
    }
    out
}

/// Burned in by default for 9:16 and 4:5 (spec §5: the two short-form
/// aspects), off otherwise; `explicit` (the script's `output.burn_captions`)
/// always overrides. A script with neither `aspect` nor `tier` set never
/// reaches this function with its own default -- the player keeps that
/// legacy case always-on (F6) -- so this is only the new, per-aspect rule.
#[must_use]
pub fn burn_in_default(aspect: Aspect, explicit: Option<bool>) -> bool {
    explicit.unwrap_or(aspect.is_portrait_like())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_story_with_chapters_and_no_captions_writes_valid_empty_srt_and_vtt() {
        let cues = track(&[], 10.0);
        assert!(cues.is_empty());
        assert_eq!(srt(&cues), "");
        assert_eq!(vtt(&cues), "WEBVTT\n\n");
        // and the bundle (Task 16) writes both files and a transcript with every chapter: asserted there by name.
    }

    #[test]
    fn srt_and_vtt_parse_back_to_the_track() {
        let inputs =
            vec![(1.0, 3.0, "Tokyo".to_string()), (2.5, 4.0, "a <b> & c --> d".to_string()), (0.0, 0.0, "dropped".to_string())];
        let cues = track(&inputs, 10.0);
        assert_eq!(cues.len(), 2);

        // SRT has no escaping of its own: a literal `-->` in the source
        // text is written as a plain `->` (the controller's ruling), a
        // lossy, one-way rewrite -- so the SRT round trip is checked
        // against that rewritten text, not the original.
        let parsed_srt = parse_srt(&srt(&cues));
        assert_eq!(parsed_srt.len(), cues.len());
        for (cue, (from, to, text)) in cues.iter().zip(parsed_srt.iter()) {
            assert_eq!(whole_ms(*from), whole_ms(cue.from), "{:?}", cue);
            assert_eq!(whole_ms(*to), whole_ms(cue.to), "{:?}", cue);
            assert_eq!(*text, cue.text.replace("-->", "->"), "{:?}", cue);
        }

        // VTT entity-escapes `&`, `<` and `-->`, so it round-trips exactly.
        let parsed_vtt = parse_vtt(&vtt(&cues));
        assert_eq!(parsed_vtt.len(), cues.len());
        for (cue, (from, to, text)) in cues.iter().zip(parsed_vtt.iter()) {
            assert_eq!(whole_ms(*from), whole_ms(cue.from), "{:?}", cue);
            assert_eq!(whole_ms(*to), whole_ms(cue.to), "{:?}", cue);
            assert_eq!(*text, cue.text, "{:?}", cue);
        }
    }

    #[test]
    fn timestamps_round_to_milliseconds_and_cross_the_hour() {
        let cues = track(&[(3599.9995, 3661.25, "x".into())], 4000.0);
        assert!(srt(&cues).contains("01:00:00,000 --> 01:01:01,250"), "{}", srt(&cues));
        assert!(vtt(&cues).contains("01:00:00.000 --> 01:01:01.250"), "{}", vtt(&cues));
    }

    #[test]
    fn vertical_stories_burn_captions_in_by_default() {
        assert!(burn_in_default(Aspect::Portrait, None));
        assert!(burn_in_default(Aspect::Feed, None));
        assert!(!burn_in_default(Aspect::Landscape, None));
        assert!(!burn_in_default(Aspect::Square, None));
        assert!(!burn_in_default(Aspect::Portrait, Some(false)));
        assert!(burn_in_default(Aspect::Landscape, Some(true)));
    }

    #[test]
    fn overlapping_cues_are_kept_in_from_order() {
        let inputs = vec![(0.0, 5.0, "a".to_string()), (1.0, 3.0, "b".to_string())];
        let cues = track(&inputs, 10.0);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "a");
        assert_eq!(cues[1].text, "b");
        assert_eq!(cues[0].index, 1);
        assert_eq!(cues[1].index, 2);
    }

    #[test]
    fn ties_at_the_same_from_keep_input_order() {
        let inputs = vec![(0.0, 1.0, "first".to_string()), (0.0, 2.0, "second".to_string())];
        let cues = track(&inputs, 10.0);
        assert_eq!(cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
    }

    #[test]
    fn clamped_to_duration_and_negative_times() {
        let inputs = vec![(-5.0, 2.0, "clamped start".to_string()), (8.0, 20.0, "clamped end".to_string())];
        let cues = track(&inputs, 10.0);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].from, 0.0);
        assert_eq!(cues[1].to, 10.0);
    }

    // --- A minimal, test-only SRT/VTT reader: just enough to round-trip
    // what this module writes, so the parse-back test above is real
    // parsing rather than a restatement of `srt`/`vtt`'s own formatting.

    fn parse_timestamp(s: &str, sep: char) -> f64 {
        let mut parts = s.splitn(3, ':');
        let h: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
        let m: f64 = parts.next().unwrap_or("0").parse().unwrap_or(0.0);
        let rest = parts.next().unwrap_or("0");
        let mut sm = rest.splitn(2, sep);
        let s_part: f64 = sm.next().unwrap_or("0").parse().unwrap_or(0.0);
        let ms: f64 = sm.next().unwrap_or("0").parse().unwrap_or(0.0);
        h * 3600.0 + m * 60.0 + s_part + ms / 1000.0
    }

    fn unescape_vtt(text: &str) -> String {
        text.replace("--&gt;", "-->").replace("&lt;", "<").replace("&amp;", "&")
    }

    /// Blocks are separated by a blank line; each block is
    /// `index\ntiming\ntext...`. No unescaping: SRT text is plain (a
    /// source `-->` already came out of `srt()` as a literal `->`).
    fn parse_srt(doc: &str) -> Vec<(f64, f64, String)> {
        let mut out = Vec::new();
        for block in doc.split("\n\n") {
            let mut lines = block.lines();
            let Some(_index) = lines.next() else { continue };
            let Some(timing) = lines.next() else { continue };
            let Some((from, to)) = timing.split_once(" --> ") else { continue };
            let text: Vec<&str> = lines.collect();
            out.push((parse_timestamp(from, ','), parse_timestamp(to, ','), text.join("\n")));
        }
        out
    }

    /// `"WEBVTT"` header block, then one block per cue, same shape as SRT
    /// but with no index line and a `.` millisecond separator.
    fn parse_vtt(doc: &str) -> Vec<(f64, f64, String)> {
        let mut out = Vec::new();
        for block in doc.split("\n\n") {
            let mut lines = block.lines();
            let Some(first) = lines.next() else { continue };
            if first.trim() == "WEBVTT" || !first.contains("-->") {
                continue;
            }
            let Some((from, to)) = first.split_once(" --> ") else { continue };
            let text: Vec<&str> = lines.collect();
            out.push((parse_timestamp(from, '.'), parse_timestamp(to, '.'), unescape_vtt(&text.join("\n"))));
        }
        out
    }
}
