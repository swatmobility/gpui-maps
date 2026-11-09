//! Chapters: a script's `chapters` resolved into story seconds against a
//! [`CameraPath`] (spec §2.1, plan Task 3).
//!
//! **Pure.** [`resolve`] reads no clock and depends only on `script` and
//! `path`'s already-resolved segment times, so it gives a bit-identical
//! result whatever else has run first.
//!
//! **Labels are `script.rs`'s job.** A chapter's `from`/`to` label is
//! checked to name a real camera segment at parse time
//! (`script::validate_chapter_time`'s `unknown_label`); by the time a
//! [`Script`] reaches [`resolve`], every label it carries is real. This
//! file only resolves those labels to seconds and checks what needs
//! resolved seconds to check: overlap, an empty or past-end span, and a
//! still outside its chapter.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use super::camera_path::CameraPath;
use super::script::{ChapterTime, Diagnostic, FurnitureSpec, Script, Severity};

/// A story falls back to this title when it names none and has no
/// `chapters` (spec §2.1's implicit chapter).
const UNTITLED_STORY: &str = "Story";

/// The implicit chapter's id, when a script names no `chapters`.
const IMPLICIT_CHAPTER_ID: &str = "story";

/// One resolved chapter (spec §2.1): a named, non-overlapping span of
/// story seconds with its own alt text, still time and (optionally)
/// furniture override.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Chapter {
    pub id: String,
    pub title: String,
    pub from: f64,
    pub to: f64,
    pub alt: Option<String>,
    /// `still_at` or the midpoint `(from + to) / 2`.
    pub still_t: f64,
    pub furniture: Option<FurnitureSpec>,
    /// `true` for the one chapter a script with no `chapters` gets,
    /// covering `[0, duration]`.
    pub implicit: bool,
}

fn error(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path: path.into(), code, message: message.into(), hint: None }
}

fn warning(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic { severity: Severity::Warning, path: path.into(), code, message: message.into(), hint: None }
}

/// `time` resolved to story seconds against `path`: a `from` is the
/// labelled segment's START and a `to` is its END (spec §2.1).
/// `None` only for a label `script.rs` did not already validate to exist
/// (never reachable through [`super::parse`]); `resolve` diagnoses it
/// defensively rather than indexing blind.
fn resolve_time(time: &ChapterTime, path: &CameraPath, end: bool) -> Option<f64> {
    match time {
        ChapterTime::Seconds(s) => Some(*s),
        ChapterTime::Label(label) => path.segment_span(label).map(|(start, finish)| if end { finish } else { start }),
    }
}

/// Resolve every chapter of `script` against `path`. A script with no
/// `chapters` gets one implicit chapter covering `[0, duration]`, titled
/// with the script's own title (or `"Story"`). Every chapter's `alt` is
/// its own non-blank `alt`, else the script's non-blank `summary`, else
/// `None`.
///
/// Diagnostics: an unresolved label (`unknown_label`, defensive only --
/// `script.rs` already refuses one at parse time), a `to` at or before its
/// `from` (`chapter_empty`), a `still_at` outside `[from, to]`
/// (`still_outside_chapter`), two chapters whose spans overlap
/// (`chapter_overlap`, on the later-starting one, at its `from`) are all
/// errors. A `to` past the path's duration (`chapter_past_end`) is a
/// warning: the chapter is clamped to the duration and resolution still
/// succeeds.
///
/// Returns every resolved chapter, in document order, alongside every
/// diagnostic found (errors AND warnings) -- not a `Result`, so a warning
/// is never lost just because nothing else went wrong.
pub fn resolve(script: &Script, path: &CameraPath) -> (Vec<Chapter>, Vec<Diagnostic>) {
    let duration = path.duration();
    // Spec §5 / `ChapterSpec::alt`: a chapter's own alt, else the story's
    // summary; blank text is none. Applied once here so the transcript,
    // the manifest's coverage and validation all see the same alt.
    let non_blank = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    let alt_of = |own: &Option<String>| non_blank(own).or_else(|| non_blank(&script.summary));
    if script.chapters.is_empty() {
        let title = script.title.clone().unwrap_or_else(|| UNTITLED_STORY.to_string());
        let chapter = Chapter {
            id: IMPLICIT_CHAPTER_ID.to_string(),
            title,
            from: 0.0,
            to: duration,
            alt: alt_of(&None),
            still_t: duration / 2.0,
            furniture: None,
            implicit: true,
        };
        return (vec![chapter], Vec::new());
    }

    let mut diags = Vec::new();
    let mut chapters: Vec<Chapter> = Vec::with_capacity(script.chapters.len());
    for (i, spec) in script.chapters.iter().enumerate() {
        let path_prefix = format!("/chapters/{i}");
        let from = resolve_time(&spec.from, path, false).unwrap_or_else(|| {
            diags.push(error(format!("{path_prefix}/from"), "unknown_label", format!("chapter `{}`'s `from` names no camera segment", spec.id)));
            0.0
        });
        let mut to = resolve_time(&spec.to, path, true).unwrap_or_else(|| {
            diags.push(error(format!("{path_prefix}/to"), "unknown_label", format!("chapter `{}`'s `to` names no camera segment", spec.id)));
            duration
        });
        if to > duration {
            diags.push(warning(
                format!("{path_prefix}/to"),
                "chapter_past_end",
                format!("chapter `{}`'s `to` ({to} s) is past the story's end at {duration} s; clamped", spec.id),
            ));
            to = duration;
        }
        if to <= from {
            diags.push(error(
                format!("{path_prefix}/to"),
                "chapter_empty",
                format!("chapter `{}` has `to` ({to} s) at or before `from` ({from} s)", spec.id),
            ));
        }
        let still_t = spec.still_at.unwrap_or((from + to) / 2.0);
        if still_t < from || still_t > to {
            diags.push(error(
                format!("{path_prefix}/still_at"),
                "still_outside_chapter",
                format!("chapter `{}`'s still at {still_t} s falls outside its span [{from}, {to}]", spec.id),
            ));
        }
        chapters.push(Chapter {
            id: spec.id.clone(),
            title: spec.title.clone(),
            from,
            to,
            alt: alt_of(&spec.alt),
            still_t,
            furniture: spec.furniture.clone(),
            implicit: false,
        });
    }

    // Overlap: checked in chronological (`from`) order, but every
    // diagnostic names the later-starting chapter's own document index.
    let mut order: Vec<(usize, &Chapter)> = chapters.iter().enumerate().collect();
    order.sort_by(|(_, a), (_, b)| a.from.total_cmp(&b.from));
    for pair in order.windows(2) {
        let [(_, a), (b_index, b)] = pair else { continue };
        if b.from < a.to {
            diags.push(error(
                format!("/chapters/{b_index}/from"),
                "chapter_overlap",
                format!("chapter `{}` (from {} s) overlaps chapter `{}` ({} s to {} s)", b.id, b.from, a.id, a.from, a.to),
            ));
        }
    }

    (chapters, diags)
}

/// The chapter containing `t`: `from <= t < to`, except the
/// latest-ending chapter, which also includes its own `to`. `None` in a
/// gap between chapters (gaps are allowed) or past every chapter's end.
pub fn chapter_at(chapters: &[Chapter], t: f64) -> Option<&Chapter> {
    let max_to = chapters.iter().map(|c| c.to).fold(f64::NEG_INFINITY, f64::max);
    chapters.iter().find(|c| c.from <= t && (t < c.to || (t <= c.to && c.to == max_to)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::story::script::ChapterSpec;
    use crate::story::test_util::test_script;
    use crate::story::tracks::TrackSet;

    fn chapter(id: &str, from: ChapterTime, to: ChapterTime) -> ChapterSpec {
        ChapterSpec { id: id.to_string(), title: id.to_string(), from, to, alt: None, still_at: None, furniture: None, card: None }
    }

    /// `resolve`'s success half: asserts no `Severity::Error` diagnostic
    /// and returns the chapters, so a test reads like the `Result` the
    /// brief first sketched (F13's ruling: `resolve` is a plain tuple, not
    /// a `Result`, so `chapter_past_end` survives even when nothing else
    /// is wrong).
    fn ok(script: &Script, path: &CameraPath) -> Vec<Chapter> {
        let (chapters, diags) = resolve(script, path);
        assert!(diags.iter().all(|d| d.severity != Severity::Error), "unexpected error(s): {diags:?}");
        chapters
    }

    #[test]
    fn labels_resolve_to_segment_start_and_end_and_no_chapters_is_one_implicit_chapter() {
        let script = test_script(serde_json::json!([
            {"type": "jump", "label": "a", "to": {"lon": 0.0, "lat": 0.0, "zoom": 3}},
            {"type": "hold", "label": "b", "duration": 4.0},
            {"type": "hold", "label": "c", "duration": 6.0}]));
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        let implicit = ok(&script, &path);
        assert_eq!(implicit.len(), 1);
        assert!(implicit[0].implicit && implicit[0].from == 0.0 && (implicit[0].to - 10.0).abs() < 1e-9);

        let mut s = script.clone();
        s.chapters = vec![
            chapter("one", ChapterTime::Label("b".into()), ChapterTime::Label("b".into())),
            chapter("two", ChapterTime::Seconds(5.0), ChapterTime::Label("c".into())),
        ];
        let c = ok(&s, &path);
        assert_eq!((c[0].from, c[0].to, c[0].still_t), (0.0, 4.0, 2.0));
        assert_eq!((c[1].from, c[1].to), (5.0, 10.0));
        assert!(chapter_at(&c, 4.5).is_none(), "a gap between chapters is allowed");
    }

    #[test]
    fn overlapping_chapters_are_an_error() {
        let mut s = test_script(serde_json::json!([
            {"type": "jump", "to": {"lon": 0.0, "lat": 0.0, "zoom": 3}}, {"type": "hold", "duration": 10.0}]));
        s.chapters =
            vec![chapter("a", ChapterTime::Seconds(0.0), ChapterTime::Seconds(5.0)), chapter("b", ChapterTime::Seconds(4.0), ChapterTime::Seconds(8.0))];
        let path = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).unwrap();
        let (_, err) = resolve(&s, &path);
        assert!(err.iter().any(|d| d.code == "chapter_overlap" && d.path == "/chapters/1/from"), "{err:?}");
    }

    #[test]
    fn a_chapter_past_the_end_warns_and_is_clamped_not_an_error() {
        let s_base = test_script(serde_json::json!([
            {"type": "jump", "to": {"lon": 0.0, "lat": 0.0, "zoom": 3}}, {"type": "hold", "duration": 10.0}]));
        let path = CameraPath::new((800.0, 600.0), &s_base, &TrackSet::default()).unwrap();
        let mut s = s_base;
        s.chapters = vec![chapter("a", ChapterTime::Seconds(0.0), ChapterTime::Seconds(15.0))];
        let (chapters, diags) = resolve(&s, &path);
        assert_eq!(chapters[0].to, 10.0, "clamped to the path's duration");
        assert!(diags.iter().any(|d| d.code == "chapter_past_end" && d.severity == Severity::Warning && d.path == "/chapters/0/to"), "{diags:?}");
        assert!(diags.iter().all(|d| d.severity != Severity::Error), "a warning alone must not look like a failure: {diags:?}");
    }

    #[test]
    fn an_empty_span_and_a_still_outside_it_are_errors() {
        let s_base = test_script(serde_json::json!([
            {"type": "jump", "to": {"lon": 0.0, "lat": 0.0, "zoom": 3}}, {"type": "hold", "duration": 10.0}]));
        let path = CameraPath::new((800.0, 600.0), &s_base, &TrackSet::default()).unwrap();
        let mut s = s_base.clone();
        s.chapters = vec![chapter("a", ChapterTime::Seconds(5.0), ChapterTime::Seconds(5.0))];
        let (_, diags) = resolve(&s, &path);
        assert!(diags.iter().any(|d| d.code == "chapter_empty" && d.path == "/chapters/0/to"), "{diags:?}");

        let mut s = s_base;
        let mut c = chapter("a", ChapterTime::Seconds(0.0), ChapterTime::Seconds(5.0));
        c.still_at = Some(9.0);
        s.chapters = vec![c];
        let (_, diags) = resolve(&s, &path);
        assert!(diags.iter().any(|d| d.code == "still_outside_chapter" && d.path == "/chapters/0/still_at"), "{diags:?}");
    }

    #[test]
    fn chapter_at_includes_the_last_chapters_own_end() {
        let s_base = test_script(serde_json::json!([
            {"type": "jump", "to": {"lon": 0.0, "lat": 0.0, "zoom": 3}}, {"type": "hold", "duration": 10.0}]));
        let path = CameraPath::new((800.0, 600.0), &s_base, &TrackSet::default()).unwrap();
        let mut s = s_base;
        s.chapters = vec![chapter("a", ChapterTime::Seconds(0.0), ChapterTime::Seconds(5.0)), chapter("b", ChapterTime::Seconds(5.0), ChapterTime::Seconds(10.0))];
        let c = ok(&s, &path);
        assert_eq!(chapter_at(&c, 0.0).map(|c| c.id.as_str()), Some("a"));
        assert_eq!(chapter_at(&c, 5.0).map(|c| c.id.as_str()), Some("b"), "b starts where a ends");
        assert_eq!(chapter_at(&c, 10.0).map(|c| c.id.as_str()), Some("b"), "the last chapter includes its own to");
        assert_eq!(chapter_at(&c, 10.001), None);
    }
}
