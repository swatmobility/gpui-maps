//! **Cards over time** (spec §3.1, §3.2, §3.5, §3.6, §10.1; Task R6): which
//! card is on screen at story time `t`, and how it got there.
//!
//! - [`card_spans`]: every card of a script as a span of story seconds --
//!   a chapter's `card` over its chapter, an event's `card` over
//!   `[at, until]` -- with the event winning over the chapter for its span
//!   (the chapter card fills the rest) and two event cards that overlap the
//!   error `cards_overlap` (§10.1's one-card rule).
//! - [`card_at`]: the card at `t`: its fade (§3.1's `fade_s`), the
//!   card-to-card ease where one card starts exactly where another ends,
//!   a caption cue in the card's caption row (or a caption-only card when
//!   no card is on screen, §3.5), and the moving-camera rule (§3.2: during
//!   a `fly` or `ease` only the caption row, eased in and out over 0.3 s at
//!   each boundary, as [`CardContent::compact`]).
//! - [`sources_at`]: the sources line's groups (§3.6): the author's, then
//!   each photo credit as `Photo`, then each overlay attribution as `Data`;
//!   a repeated group once. [`crate::story::frame_layout::dedupe_sources`]
//!   then drops what the tile credit already says, once, at layout.
//!
//! Everything is a pure function of the script and `t`: no clock, no
//! state carried from the previous frame, so a seek lands on the frame
//! playing reaches. The camera pass ([`crate::story::camera_path::CardBoxes`])
//! and the drawer read the same spans, so they cannot disagree about which
//! card is on screen. Shared with the browser and iOS by `#[path]`.

use crate::story::camera_path::{CameraPath, Kind};
use crate::story::captions::Cue;
use crate::story::card_layout::{CardContent, CARD_EASE_S};
use crate::story::chapters::Chapter;
use crate::story::frame_layout::Dir;
use crate::story::script::{Diagnostic, EventAction, EventTime, Script, Severity, SourceGroup};

/// The default card fade, seconds (spec §3.1's `fade_s`).
pub const CARD_FADE_S: f64 = 0.3;

/// One card on screen over `[from, to)` story seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSpan {
    /// The chapter's id, or `events[<i>]` for an event card.
    pub id: String,
    pub from: f64,
    pub to: f64,
    pub content: CardContent,
    /// Its fade in and out, seconds.
    pub fade_s: f64,
    /// Whether it draws the progress hairline (`progress` not `"off"`).
    pub progress: bool,
}

/// The card [`card_at`] puts on screen at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct CardAt {
    pub card: CardContent,
    /// The card being replaced while the plate eases (spec §3.1, §3.2):
    /// `Some` only inside the 0.3 s ease, with [`CardAt::ease`] below 1.
    pub from_card: Option<CardContent>,
    /// The ease's linear progress, 0..1; 1 when settled.
    pub ease: f32,
    /// The card's fade, 0..1.
    pub alpha: f32,
    /// Whether the hairline shows (the span's `progress`).
    pub progress: bool,
}

/// An event's `at` or `until` in story seconds against `path`, `None` for
/// a label the path does not have (the timeline diagnoses it).
fn event_time(time: &EventTime, path: &CameraPath) -> Option<f64> {
    let s = match time {
        EventTime::At(s) => Some(*s),
        EventTime::After { after, offset } => path.label_time(after, false).map(|s| s + offset),
        EventTime::AtStart { at_start, offset } => path.label_time(at_start, true).map(|s| s + offset),
    }?;
    (s.is_finite() && s >= 0.0).then_some(s)
}

fn span_of(id: String, from: f64, to: f64, spec: &crate::story::script::CardSpec, script: &Script) -> Option<CardSpan> {
    let content = CardContent::from_spec_in(spec, script);
    (!content.is_empty() && to > from).then(|| CardSpan {
        id,
        from,
        to,
        content,
        fade_s: spec.fade_s.filter(|f| f.is_finite() && *f >= 0.0).unwrap_or(CARD_FADE_S),
        progress: spec.progress.as_deref() != Some("off"),
    })
}

/// Every card of `script` as a span over `path` (pass 1: the design camera
/// pass keeps every boundary), sorted by `from`, and the `cards_overlap`
/// errors (spec §10.1).
///
/// - A chapter's `card` spans its chapter.
/// - An event's `card` spans `[at, until]`, to the end of the story with
///   no `until`.
/// - Where an event card and a chapter card overlap, the event card wins
///   for its span and the chapter card fills the rest of its chapter.
/// - Two event cards that overlap are an error on the later-starting one,
///   which is left out (one card at a time).
#[must_use]
pub fn card_spans(script: &Script, path: &CameraPath) -> (Vec<CardSpan>, Vec<Diagnostic>) {
    let (chapters, _) = crate::story::chapters::resolve(script, path);
    card_spans_in(script, &chapters, path)
}

/// [`card_spans`] over chapters already resolved against `path`.
#[must_use]
pub fn card_spans_in(script: &Script, chapters: &[Chapter], path: &CameraPath) -> (Vec<CardSpan>, Vec<Diagnostic>) {
    let duration = path.duration();
    let mut diags = Vec::new();
    let mut events: Vec<(usize, CardSpan)> = script
        .events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let EventAction::Card(spec) = &e.action else { return None };
            let from = event_time(&e.at, path)?;
            let to = e.until.as_ref().map_or(Some(duration), |u| event_time(u, path))?.min(duration);
            span_of(format!("events[{i}]"), from, to, spec, script).map(|s| (i, s))
        })
        .collect();
    events.sort_by(|a, b| a.1.from.total_cmp(&b.1.from).then(a.0.cmp(&b.0)));
    let mut kept: Vec<CardSpan> = Vec::new();
    for (i, span) in events {
        if let Some(last) = kept.iter().find(|k| k.from < span.to && span.from < k.to) {
            diags.push(Diagnostic {
                severity: Severity::Error,
                path: format!("/events/{i}/card"),
                code: "cards_overlap",
                // Spec §11: the time first, then what happened and what to do.
                message: format!(
                    "at {:.2} s: Two cards are on screen at once ({} until {:.2} s, and {}). A story shows one card at a time; end one before the next starts.",
                    span.from, last.id, last.to, span.id
                ),
                hint: None,
            });
            continue;
        }
        kept.push(span);
    }
    let mut spans = kept.clone();
    for c in chapters {
        let Some(spec) = script.chapters.iter().find(|s| s.id == c.id).and_then(|s| s.card.as_ref()) else { continue };
        let Some(whole) = span_of(c.id.clone(), c.from, c.to, spec, script) else { continue };
        // The chapter card fills what the event cards leave of it.
        let mut cursor = whole.from;
        for e in kept.iter().filter(|e| e.to > whole.from && e.from < whole.to) {
            if e.from > cursor {
                spans.push(CardSpan { from: cursor, to: e.from, ..whole.clone() });
            }
            cursor = cursor.max(e.to);
        }
        if cursor < whole.to {
            spans.push(CardSpan { from: cursor, ..whole });
        }
    }
    spans.sort_by(|a, b| a.from.total_cmp(&b.from));
    (spans, diags)
}

/// The card each stretch of time shows with the caption cues applied
/// (spec §3.5), before the moving-camera rule: `spans` cut at every cue's
/// ends, a cue's text taking row 2 (ruling R6-a: the cue replaces the
/// caption and the body yields to it, returning when the cue ends; R6-b:
/// under a headline or quote it is set in the body's style), and a
/// cue with no card on screen a caption-only card. Neighbouring stretches
/// that show the same card are one span.
///
/// The ONE cut: [`card_at`] draws from it and the camera pass
/// ([`crate::story::camera_path::CardBoxes`]) lays its boxes out from it,
/// so the camera clears the card height the frame draws.
#[must_use]
pub fn screen_spans(spans: &[CardSpan], cues: &[Cue]) -> Vec<CardSpan> {
    let mut bounds: Vec<f64> =
        spans.iter().flat_map(|s| [s.from, s.to]).chain(cues.iter().flat_map(|c| [c.from, c.to])).filter(|b| b.is_finite()).collect();
    bounds.sort_by(f64::total_cmp);
    bounds.dedup();
    let mut out: Vec<CardSpan> = Vec::new();
    for w in bounds.windows(2) {
        let (from, to) = (w[0], w[1]);
        let x = (from + to) / 2.0;
        let within = |a: f64, b: f64| a <= x && x < b;
        let cue = cues.iter().enumerate().find(|(_, c)| within(c.from, c.to));
        let mut span = match spans.iter().find(|s| within(s.from, s.to)) {
            Some(s) => CardSpan { from, to, ..s.clone() },
            None => match cue {
                Some((k, _)) => CardSpan { id: format!("captions[{k}]"), from, to, content: CardContent::default(), fade_s: CARD_FADE_S, progress: false },
                None => continue,
            },
        };
        if let Some((_, c)) = cue {
            let text = c.text.trim().to_string();
            // Ruling R6-b: under a row 1 (a headline or a quote) the cue is
            // set as the body it replaces -- body size, weight and ink -- so
            // the card does not read as two headlines. It stays the caption
            // too, which is what the compact card (§3.2) shows. A
            // caption-only card keeps the caption style.
            let row1 = span.content.headline.is_some() || span.content.quote.is_some();
            span.content.body = row1.then(|| text.clone());
            span.content.caption = Some(text);
        }
        match out.last_mut() {
            Some(last) if last.to == from && last.content == span.content && last.fade_s == span.fade_s && last.progress == span.progress => last.to = to,
            _ => out.push(span),
        }
    }
    out
}

/// The `fly` and `ease` segments of `path` as `(start, end)` story seconds:
/// where the moving-camera rule (spec §3.2) compacts the card.
#[must_use]
pub fn moving_spans(path: &CameraPath) -> Vec<(f64, f64)> {
    path.segments()
        .iter()
        .filter(|s| matches!(s.kind, Kind::Fly(_) | Kind::Ease(_)) && s.duration > 0.0)
        .map(|s| (s.start, s.start + s.duration))
        .collect()
}

/// What is on screen over one stretch of time: the card as drawn, with
/// the fade and hairline of the span it comes from.
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    content: CardContent,
    fade_s: f64,
    progress: bool,
}

fn piece_at(screen: &[CardSpan], moving: &[(f64, f64)], x: f64) -> Option<Piece> {
    let within = |from: f64, to: f64| from <= x && x < to;
    let s = screen.iter().find(|s| within(s.from, s.to))?;
    let mut content = s.content.clone();
    content.compact = moving.iter().any(|(a, b)| within(*a, *b));
    Some(Piece { content, fade_s: s.fade_s, progress: s.progress })
}

/// The card on screen at `t` (spec §3.1, §3.2, §3.5), `None` for none.
///
/// The story's time is cut where anything changes what the card shows:
/// the ends of [`screen_spans`] (cards, cues) and of each moving segment.
/// A stretch whose neighbour also shows a card is joined to it: the plate
/// stays and eases from the neighbour's card over [`CARD_EASE_S`] -- a
/// line both draw holds, the rest cross-fades
/// ([`crate::story::card_layout::ease_cards`]). A stretch with no card
/// beside it fades in or out over its span's `fade_s`. So a card fades in
/// once, compacts when a `fly` starts and grows back when it ends, swaps
/// row 2 for a cue, and gives way to the next card, all as pure functions
/// of `t`.
///
/// Where no card is on screen (a gap between chapters, after the last
/// card) this is `None`, so the frame carries no card at all -- not an
/// invisible one -- and a 1a item of a design story is not pushed aside
/// by a card nobody sees.
#[must_use]
pub fn card_at(spans: &[CardSpan], cues: &[Cue], moving: &[(f64, f64)], t: f64) -> Option<CardAt> {
    if !t.is_finite() {
        return None;
    }
    let screen = screen_spans(spans, cues);
    let here = piece_at(&screen, moving, t)?;
    let mut bounds: Vec<f64> = screen
        .iter()
        .flat_map(|s| [s.from, s.to])
        .chain(moving.iter().flat_map(|(a, b)| [*a, *b]))
        .filter(|b| b.is_finite())
        .collect();
    bounds.sort_by(f64::total_cmp);
    bounds.dedup();
    // The stretch `[bounds[k - 1], bounds[k])` holds `t`: something is on
    // screen at `t`, so a span starts at or before it and ends after it.
    let k = bounds.partition_point(|b| *b <= t);
    if k == 0 || k >= bounds.len() {
        return None;
    }
    let between = |i: usize| piece_at(&screen, moving, (bounds[i] + bounds[i + 1]) / 2.0);
    let mut first = k - 1;
    while first > 0 && between(first - 1).as_ref() == Some(&here) {
        first -= 1;
    }
    let mut last = k - 1;
    while last + 2 < bounds.len() && between(last + 1).as_ref() == Some(&here) {
        last += 1;
    }
    let (start, end) = (bounds[first], bounds[last + 1]);
    let before = (first > 0).then(|| between(first - 1)).flatten();
    let after = (last + 2 < bounds.len()).then(|| between(last + 1)).flatten();
    let fade = here.fade_s.max(1e-6);
    let (mut from_card, mut ease) = (None, 1.0_f32);
    let fade_in = match before {
        Some(prev) => {
            // Card to card (spec §3.1): the plate stays and eases.
            let s = (t - start) / CARD_EASE_S;
            if s < 1.0 {
                from_card = Some(prev.content);
                ease = s.clamp(0.0, 1.0) as f32;
            }
            1.0
        }
        None => (t - start) / fade,
    };
    let fade_out = if after.is_some() { 1.0 } else { (end - t) / fade };
    Some(CardAt { card: here.content, from_card, ease, alpha: fade_in.min(fade_out).clamp(0.0, 1.0) as f32, progress: here.progress })
}

/// The hairline's filled fraction at `t` (spec §3.3): `(i - 1 + p) / n`
/// over the `n` chapters, `p` through the chapter at `t` (the last one
/// begun when `t` is in none). `None` with one chapter or none.
#[must_use]
pub fn progress_at(chapters: &[Chapter], t: f64) -> Option<f32> {
    let n = chapters.len();
    if n < 2 {
        return None;
    }
    let i = chapters.iter().position(|c| t >= c.from && t < c.to).or_else(|| chapters.iter().rposition(|c| t >= c.from))?;
    let c = &chapters[i];
    let p = if c.to > c.from { ((t - c.from) / (c.to - c.from)).clamp(0.0, 1.0) } else { 1.0 };
    Some(((i as f64 + p) / n as f64) as f32)
}

/// The sources line's groups (spec §3.6): `script`'s `furniture.sources`,
/// then each of `photo_credits` as `Photo`, then each of
/// `overlay_attributions` as `Data`; blank text left out and a repeated
/// group (same key and text) once. The tile credit is not deduplicated
/// here: [`crate::story::frame_layout::dedupe_sources`] does that, once,
/// as the row is laid out.
#[must_use]
pub fn sources_at(script: &Script, photo_credits: &[String], overlay_attributions: &[String]) -> Vec<SourceGroup> {
    sources_from(script.furniture.sources.as_deref().unwrap_or_default(), photo_credits, overlay_attributions)
}

/// [`sources_at`] with the author's groups given.
#[must_use]
pub fn sources_from(author: &[SourceGroup], photo_credits: &[String], overlay_attributions: &[String]) -> Vec<SourceGroup> {
    let group = |key: &str, text: &String| {
        let text = text.trim();
        (!text.is_empty()).then(|| SourceGroup { key: key.into(), text: text.to_string() })
    };
    let mut out: Vec<SourceGroup> = author
        .iter()
        .cloned()
        .chain(photo_credits.iter().filter_map(|c| group("Photo", c)))
        .chain(overlay_attributions.iter().filter_map(|a| group("Data", a)))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|g| seen.insert((g.key.clone(), g.text.clone())));
    out
}

/// The story's reading direction (spec §9): `design.dir` (`"ltr"` or
/// `"rtl"`) when the author set it, else from `lang`: a primary language
/// subtag of `ar`, `he`, `fa` or `ur` is right-to-left, anything else
/// (or no `lang`) left-to-right. An unknown `design.dir` falls back to
/// `lang`; validation warns `unknown_dir` (spec §11).
#[must_use]
pub fn dir_for(lang: Option<&str>, design_dir: Option<&str>) -> Dir {
    match design_dir.map(|d| d.trim().to_ascii_lowercase()).as_deref() {
        Some("rtl") => return Dir::Rtl,
        Some("ltr") => return Dir::Ltr,
        _ => {}
    }
    let primary = lang.and_then(|l| l.trim().split(['-', '_']).next()).map(str::to_ascii_lowercase);
    match primary.as_deref() {
        Some("ar" | "he" | "fa" | "ur") => Dir::Rtl,
        _ => Dir::Ltr,
    }
}

/// U+2068 FIRST STRONG ISOLATE and U+2069 POP DIRECTIONAL ISOLATE.
pub const FSI: char = '\u{2068}';
pub const PDI: char = '\u{2069}';

/// U+2066 LRI, U+2067 RLI or U+2068 FSI: an isolate initiator.
pub(crate) fn is_isolate_opener(c: char) -> bool {
    matches!(c, '\u{2066}' | '\u{2067}' | FSI)
}

/// `lines` (one paragraph wrapped) with its isolates balanced per line
/// (spec §9, R14 review): an isolate a line leaves open is closed with a
/// PDI at the line's end and reopened, with the same initiator, at the
/// next line's start. Each drawn line is shaped as its own paragraph, and
/// its base direction is its first strong character outside isolates; a
/// line that began inside the tail of `FSI Marina Bay PDI` would otherwise start
/// on Latin `Bay` and flip to LTR. A stray PDI (no opener on the line or
/// carried in) is dropped. Lines with no isolate character are unchanged.
#[must_use]
pub fn balance_isolates(lines: Vec<String>) -> Vec<String> {
    if !lines.iter().any(|l| l.chars().any(|c| is_isolate_opener(c) || c == PDI)) {
        return lines;
    }
    let mut open: Vec<char> = Vec::new();
    lines
        .into_iter()
        .map(|line| {
            let mut out: String = open.iter().collect();
            for c in line.chars() {
                if is_isolate_opener(c) {
                    open.push(c);
                } else if c == PDI && open.pop().is_none() {
                    continue;
                }
                out.push(c);
            }
            out.extend(std::iter::repeat_n(PDI, open.len()));
            out
        })
        .collect()
}

/// Whether `c` is a right-to-left letter (Arabic, Hebrew and the other
/// RTL scripts a story may carry).
fn is_rtl_letter(c: char) -> bool {
    use unicode_script::{Script as S, UnicodeScript as _};
    matches!(c.script(), S::Arabic | S::Hebrew | S::Syriac | S::Thaana | S::Nko | S::Samaritan | S::Mandaic | S::Adlam)
}

/// A character that makes an LTR run: a Western digit or a Latin letter.
fn ltr_core(c: char) -> bool {
    use unicode_script::{Script as S, UnicodeScript as _};
    c.is_ascii_digit() || (c.is_alphabetic() && c.script() == S::Latin)
}

/// `text` with every digit and Latin run inside right-to-left text wrapped
/// in first-strong isolates (spec §9: U+2068 … U+2069), so `~16`, `358`,
/// `5′21″` or `Marina Bay` keep their own left-to-right order inside an
/// Arabic line. A run is a digit or a Latin letter, then digits, Latin
/// letters, and the joiners `. , : / ' - – × ′ ″` or a space when another
/// digit or letter follows; a sign (`~ + − - ± $ € £ #`) directly before
/// a digit opens it and `% ° ′ ″ ‰` may close it. Western digits are never
/// converted. Text with no right-to-left letter is returned unchanged.
/// A run the author (or an earlier call) already isolated -- U+2066,
/// U+2067 or U+2068 up to its matching U+2069 -- is copied as it stands
/// and the rest of the text is still isolated, so this is idempotent and
/// one hand-isolated run never switches it off for the others.
#[must_use]
pub fn isolate_ltr_runs(text: &str) -> String {
    if !text.chars().any(is_rtl_letter) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let sign = |c: char| matches!(c, '~' | '+' | '−' | '-' | '±' | '$' | '€' | '£' | '#');
    let joiner = |c: char| matches!(c, '.' | ',' | ':' | '/' | '\'' | '-' | '–' | '×' | '′' | '″' | ' ');
    let closer = |c: char| matches!(c, '%' | '°' | '′' | '″' | '‰');
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_isolate_opener(c) {
            // Already isolated: copied through its matching PDI (or to the
            // end, unterminated), nested isolates included.
            let mut depth = 0usize;
            while i < chars.len() {
                let d = chars[i];
                out.push(d);
                i += 1;
                if is_isolate_opener(d) {
                    depth += 1;
                } else if d == PDI {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        let opens = ltr_core(c) || (sign(c) && chars.get(i + 1).is_some_and(char::is_ascii_digit));
        if !opens {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() {
            let c = chars[i];
            if ltr_core(c) || (joiner(c) && chars.get(i + 1).is_some_and(|n| ltr_core(*n))) {
                i += 1;
            } else if closer(c) {
                i += 1;
                break;
            } else {
                break;
            }
        }
        out.push(FSI);
        out.extend(&chars[start..i]);
        out.push(PDI);
    }
    out
}

/// `content` with [`isolate_ltr_runs`] applied to every text it draws: the
/// rows, the quote and its attribution, the stats and the legend labels.
#[must_use]
pub fn isolate_content(content: &CardContent) -> CardContent {
    use crate::story::card_layout::CardItem;
    let iso = |s: &Option<String>| s.as_deref().map(isolate_ltr_runs);
    let mut c = content.clone();
    c.headline = iso(&content.headline);
    c.body = iso(&content.body);
    c.caption = iso(&content.caption);
    if let Some(q) = c.quote.as_mut() {
        q.text = isolate_ltr_runs(&q.text);
        q.by = iso(&q.by);
        q.source = iso(&q.source);
    }
    if let Some(p) = c.photo.as_mut() {
        p.credit = iso(&p.credit);
    }
    for item in &mut c.items {
        match item {
            CardItem::Stats(stats) => {
                for s in stats {
                    s.value = isolate_ltr_runs(&s.value);
                    s.unit = iso(&s.unit);
                    s.label = isolate_ltr_runs(&s.label);
                }
            }
            CardItem::Legend { rows, title } => {
                for r in rows {
                    r.label = isolate_ltr_runs(&r.label);
                    r.value = iso(&r.value);
                }
                *title = iso(title);
            }
            CardItem::Sequential { ticks, title, .. } => {
                for t in ticks {
                    *t = isolate_ltr_runs(t);
                }
                *title = iso(title);
            }
            CardItem::Attribution => {}
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::test_util::minimal_script;
    use crate::story::tracks::TrackSet;

    /// Spec §9: `lang` sets the direction; `design.dir` overrides it.
    #[test]
    fn dir_for_reads_lang_then_design_dir() {
        for lang in ["ar", "he", "fa", "ur", "ar-EG", "AR", "he_IL", " fa-IR "] {
            assert_eq!(dir_for(Some(lang), None), Dir::Rtl, "{lang}");
        }
        for lang in ["en", "de", "de-AT", "arn", "ja", "", "urd"] {
            assert_eq!(dir_for(Some(lang), None), Dir::Ltr, "{lang}");
        }
        assert_eq!(dir_for(None, None), Dir::Ltr);
        assert_eq!(dir_for(Some("ar"), Some("ltr")), Dir::Ltr, "the author's design.dir wins");
        assert_eq!(dir_for(Some("en"), Some("rtl")), Dir::Rtl);
        assert_eq!(dir_for(None, Some(" RTL ")), Dir::Rtl);
        assert_eq!(dir_for(Some("ar"), Some("sideways")), Dir::Rtl, "an unknown dir falls back to lang");
    }

    /// Spec §9: digit and Latin runs inside RTL text are wrapped in
    /// U+2068 … U+2069; Western digits are kept; LTR text is untouched.
    #[test]
    fn isolates_wrap_digit_and_latin_runs() {
        let w = |s: &str| format!("{FSI}{s}{PDI}");
        assert_eq!(isolate_ltr_runs("يبلغ طوله ~16 كم"), format!("يبلغ طوله {} كم", w("~16")));
        assert_eq!(isolate_ltr_runs("358 كم"), format!("{} كم", w("358")));
        assert_eq!(isolate_ltr_runs("افتتح عام 2008 في Marina Bay."), format!("افتتح عام {} في {}.", w("2008"), w("Marina Bay")));
        assert_eq!(isolate_ltr_runs("الزمن 5′21″"), format!("الزمن {}", w("5′21″")));
        assert_eq!(isolate_ltr_runs("نسبة 12.5% فقط"), format!("نسبة {} فقط", w("12.5%")));
        assert_eq!(isolate_ltr_runs("רחוב 3, תל אביב"), format!("רחוב {}, תל אביב", w("3")));
        // Idempotent, and a no-op without an RTL letter.
        let once = isolate_ltr_runs("يبلغ طوله ~16 كم");
        assert_eq!(isolate_ltr_runs(&once), once);
        for ltr in ["358", "~16", "Marina Bay 2008", "Die Witwe des Kapitäns", ""] {
            assert_eq!(isolate_ltr_runs(ltr), ltr);
        }
        // A hand-isolated run is kept, and the rest is still isolated (R14
        // review): one author isolate no longer switches the function off.
        let hand = format!("بين {} و 2008", w("PUB"));
        assert_eq!(isolate_ltr_runs(&hand), format!("بين {} و {}", w("PUB"), w("2008")));
        let nested = format!("أ {FSI}x {FSI}y{PDI} z{PDI} 7");
        assert_eq!(isolate_ltr_runs(&nested), format!("أ {FSI}x {FSI}y{PDI} z{PDI} {}", w("7")), "nested isolates copied whole");
        // Balanced per line: carried, closed, reopened; a stray PDI dropped.
        let lines = balance_isolates(vec![format!("أ {FSI}Marina"), format!("Bay{PDI} ب"), format!("ج{PDI}")]);
        assert_eq!(lines, vec![format!("أ {FSI}Marina{PDI}"), format!("{FSI}Bay{PDI} ب"), "ج".to_string()]);
        assert_eq!(balance_isolates(vec!["a b".into(), "c".into()]), vec!["a b".to_string(), "c".to_string()]);
        // Arabic-Indic digits are the author's: never converted or wrapped.
        assert_eq!(isolate_ltr_runs("٣٥٨ كم"), "٣٥٨ كم");
        // Every text a card draws.
        let card = CardContent {
            headline: Some("سد مارينا 2008".into()),
            items: vec![crate::story::card_layout::CardItem::Stats(vec![crate::story::card_layout::Stat {
                value: "358".into(),
                unit: Some("م".into()),
                label: "الطول km".into(),
            }])],
            ..CardContent::default()
        };
        let iso = isolate_content(&card);
        assert_eq!(iso.headline.as_deref(), Some(format!("سد مارينا {}", w("2008")).as_str()));
        let crate::story::card_layout::CardItem::Stats(s) = &iso.items[0] else { panic!("stats") };
        assert_eq!((s[0].value.as_str(), s[0].label.clone()), ("358", format!("الطول {}", w("km"))), "a bare number has no RTL letter to isolate from");
    }

    fn path_of(script: &Script) -> CameraPath {
        CameraPath::new((1080.0, 1920.0), script, &TrackSet::default()).expect("a path")
    }

    fn spans_of(script: &Script) -> (Vec<CardSpan>, Vec<Diagnostic>) {
        card_spans(script, &path_of(script))
    }

    fn cue(from: f64, to: f64, text: &str) -> Cue {
        Cue { index: 0, from, to, text: text.into() }
    }

    fn headline(text: &str) -> CardContent {
        CardContent { headline: Some(text.into()), ..CardContent::default() }
    }

    /// Spec §10.1: an event card wins for its span; the chapter card fills
    /// the rest of the chapter, and the plate eases at each join.
    #[test]
    fn an_event_card_wins_over_its_chapter_card() {
        let script = minimal_script(serde_json::json!({
            "chapters": [{"id": "bay", "title": "Bay", "from": 0, "to": 10, "card": {"headline": "The bay"}}],
            "events": [{"at": 3, "until": 6, "card": {"headline": "The barrage"}}]
        }));
        let (spans, diags) = spans_of(&script);
        assert!(diags.is_empty(), "{diags:?}");
        let got: Vec<(&str, f64, f64)> = spans.iter().map(|s| (s.id.as_str(), s.from, s.to)).collect();
        assert_eq!(got, vec![("bay", 0.0, 3.0), ("events[0]", 3.0, 6.0), ("bay", 6.0, 10.0)]);
        let at = |t: f64| card_at(&spans, &[], &[], t).expect("a card");
        assert_eq!(at(4.5).card, headline("The barrage"));
        assert_eq!(at(4.5).alpha, 1.0);
        assert_eq!(at(8.0).card, headline("The bay"));
        // The joins ease; the story's ends fade.
        let join = at(3.1);
        assert_eq!(join.from_card, Some(headline("The bay")));
        assert!((join.ease - 1.0 / 3.0).abs() < 1e-4, "{}", join.ease);
        assert_eq!(join.alpha, 1.0, "the plate stays across a join");
        assert!((at(0.15).alpha - 0.5).abs() < 1e-4, "fades in over 0.3 s");
        assert!((at(9.85).alpha - 0.5).abs() < 1e-4, "fades out over 0.3 s");
    }

    /// Spec §10.1: one card at a time.
    #[test]
    fn overlapping_event_cards_are_an_error() {
        let script = minimal_script(serde_json::json!({
            "events": [
                {"at": 1, "until": 5, "card": {"headline": "First"}},
                {"at": 4, "until": 8, "card": {"headline": "Second"}},
                {"at": 5, "until": 9, "card": {"headline": "Touching is fine"}}
            ]
        }));
        let (spans, diags) = spans_of(&script);
        assert_eq!(diags.len(), 1, "{diags:?}");
        let d = &diags[0];
        assert_eq!((d.code, d.severity, d.path.as_str()), ("cards_overlap", Severity::Error, "/events/1/card"));
        assert!(d.message.starts_with("at 4.00 s: ") && d.message.contains("events[0] until 5.00 s") && d.message.contains("events[1]"), "{}", d.message);
        assert!(d.message.len() <= 200 && d.message.ends_with('.'), "spec §11's message rules: {}", d.message);
        let ids: Vec<&str> = spans.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["events[0]", "events[2]"], "the later overlapping card is left out");
    }

    /// Spec §3.5: a cue on screen with a card replaces that card's caption.
    #[test]
    fn a_caption_cue_replaces_the_cards_caption() {
        let card = CardContent { caption: Some("Own caption".into()), ..headline("The bay") };
        let spans = vec![CardSpan { id: "bay".into(), from: 0.0, to: 10.0, content: card.clone(), fade_s: CARD_FADE_S, progress: true }];
        let cues = [cue(2.0, 4.0, "  From the track  ")];
        let at = |t: f64| card_at(&spans, &cues, &[], t).expect("a card");
        assert_eq!(at(1.0).card, card);
        let during = at(3.0).card;
        assert_eq!(during.caption.as_deref(), Some("From the track"));
        assert_eq!(during.headline.as_deref(), Some("The bay"), "the rest of the card stays");
        assert_eq!(at(3.0).alpha, 1.0, "the plate never fades for a cue");
        assert_eq!(at(5.0).card, card, "the card's own caption is back after the cue");
    }

    /// Spec §3.5: a cue with no card on screen is a caption-only card.
    #[test]
    fn a_cue_with_no_card_draws_a_caption_only_card() {
        let cues = [cue(2.0, 5.0, "Singapore, 1965")];
        assert_eq!(card_at(&[], &cues, &[], 1.0), None, "nothing before the cue");
        let at = card_at(&[], &cues, &[], 3.0).expect("a caption-only card");
        assert_eq!(at.card, CardContent { caption: Some("Singapore, 1965".into()), ..CardContent::default() });
        assert_eq!(at.alpha, 1.0);
        assert!(!at.progress, "no hairline on a caption-only card");
        assert!((card_at(&[], &cues, &[], 2.15).unwrap().alpha - 0.5).abs() < 1e-4, "it fades in like a card");
        assert_eq!(card_at(&[], &cues, &[], 5.0), None);
        let spans = vec![CardSpan { id: "a".into(), from: 0.0, to: 3.0, content: headline("A"), fade_s: CARD_FADE_S, progress: true }];
        let screen = screen_spans(&spans, &cues);
        let cut: Vec<(f64, f64, Option<&str>, Option<&str>)> =
            screen.iter().map(|s| (s.from, s.to, s.content.headline.as_deref(), s.content.caption.as_deref())).collect();
        assert_eq!(
            cut,
            vec![(0.0, 2.0, Some("A"), None), (2.0, 3.0, Some("A"), Some("Singapore, 1965")), (3.0, 5.0, None, Some("Singapore, 1965"))],
            "the camera pass clears the same cards, cut where the cue cuts them"
        );
        let after = card_at(&spans, &cues, &[], 3.1).unwrap();
        assert_eq!(after.from_card.and_then(|c| c.headline), Some("A".into()), "the plate eases from the card to the caption");
    }

    /// Spec §3.2: during a `fly` or `ease` the card shows only its caption
    /// row; at each boundary the plate eases between compact and full over
    /// 0.3 s. A hold, a jump, an orbit or a follow shows the full card.
    #[test]
    fn a_moving_camera_shows_only_the_caption_row() {
        let script = minimal_script(serde_json::json!({
            "camera": [
                {"type": "jump", "to": {"lon": 103.851, "lat": 1.2905, "zoom": 14}},
                {"type": "hold", "duration": 2},
                {"type": "fly", "to": {"lon": 103.86, "lat": 1.30, "zoom": 13}, "duration": 3},
                {"type": "ease", "to": {"lon": 103.87, "lat": 1.30, "zoom": 13}, "duration": 2},
                {"type": "hold", "duration": 3}
            ],
            "chapters": [{"id": "all", "title": "All", "from": 0, "to": 10,
                "card": {"headline": "Head", "body": "Body", "stats": [{"value": "1", "label": "One"}]}}]
        }));
        let path = path_of(&script);
        let moving = moving_spans(&path);
        assert_eq!(moving, vec![(2.0, 5.0), (5.0, 7.0)]);
        let (spans, _) = card_spans(&script, &path);
        let at = |t: f64| card_at(&spans, &[], &moving, t).expect("a card");
        assert!(!at(1.0).card.compact, "a hold shows the full card");
        assert!(at(3.5).card.compact && at(6.0).card.compact, "a fly and an ease show the caption row");
        assert_eq!(at(3.5).from_card, None, "a fly then an ease is one compact stretch");
        assert!(!at(8.0).card.compact);
        // Into the fly: the full card eases to the compact one.
        let into = at(2.15);
        assert!(into.card.compact);
        assert_eq!(into.from_card.as_ref().map(|c| c.compact), Some(false));
        assert!((into.ease - 0.5).abs() < 1e-4);
        assert_eq!(into.alpha, 1.0, "the plate stays");
        // Out of the ease: compact back to full.
        let out = at(7.1);
        assert!(!out.card.compact);
        assert_eq!(out.from_card.as_ref().map(|c| c.compact), Some(true));
        assert_eq!(at(7.31).from_card, None, "settled after 0.3 s");
    }

    /// Spec §3.2: the plate's rectangle has no jump at a camera boundary:
    /// the frame just before it and just after it lay the card out within
    /// a hair of each other.
    #[test]
    fn an_ease_across_a_camera_boundary_is_continuous() {
        let full = CardContent { caption: Some("Caption".into()), body: Some("A body long enough to wrap".into()), ..headline("Head") };
        let spans = vec![CardSpan { id: "c".into(), from: 0.0, to: 10.0, content: full, fade_s: CARD_FADE_S, progress: true }];
        let moving = [(3.0, 6.0)];
        for b in [3.0, 6.0, 6.3] {
            let before = drawn(&card_at(&spans, &[], &moving, b - 1e-6).unwrap()).card.unwrap();
            let after = drawn(&card_at(&spans, &[], &moving, b + 1e-6).unwrap()).card.unwrap();
            for (x, y) in [(before.0, after.0), (before.1, after.1), (before.2, after.2), (before.3, after.3)] {
                assert!((x - y).abs() < 0.01, "at {b}: {before:?} -> {after:?}");
            }
        }
        let compact = drawn(&card_at(&spans, &[], &moving, 4.5).unwrap()).card.unwrap();
        let settled = drawn(&card_at(&spans, &[], &moving, 8.0).unwrap()).card.unwrap();
        assert!(compact.3 - compact.1 < settled.3 - settled.1, "the compact card is shorter");
    }

    /// Review Focus 3: every frame of a 10 s script, played forwards and
    /// then backwards, is the same frame (no state is carried).
    #[test]
    fn seeking_equals_playing_across_eases() {
        let script = minimal_script(serde_json::json!({
            "camera": [
                {"type": "jump", "to": {"lon": 103.851, "lat": 1.2905, "zoom": 14}},
                {"type": "hold", "duration": 2},
                {"type": "fly", "to": {"lon": 103.86, "lat": 1.30, "zoom": 13}, "duration": 3},
                {"type": "hold", "duration": 5}
            ],
            "chapters": [
                {"id": "a", "title": "A", "from": 0, "to": 4, "card": {"headline": "A", "caption": "a"}},
                {"id": "b", "title": "B", "from": 4, "to": 10, "card": {"headline": "B", "body": "b"}}
            ],
            "events": [{"at": 6, "until": 7.5, "card": {"headline": "Event"}}],
            "captions": [{"from": 7.4, "to": 9, "text": "Cue"}]
        }));
        let path = path_of(&script);
        let (spans, _) = card_spans(&script, &path);
        let moving = moving_spans(&path);
        let cues = [cue(7.4, 9.0, "Cue")];
        let frames: Vec<f64> = (0..300).map(|k| f64::from(k) / 30.0).collect();
        let forwards: Vec<Option<CardAt>> = frames.iter().map(|t| card_at(&spans, &cues, &moving, *t)).collect();
        let mut backwards: Vec<Option<CardAt>> = frames.iter().rev().map(|t| card_at(&spans, &cues, &moving, *t)).collect();
        backwards.reverse();
        assert_eq!(forwards, backwards);
        assert!(forwards.iter().flatten().any(|a| a.from_card.is_some()), "the script has eases");
        assert!(forwards.iter().flatten().any(|a| a.card.compact), "and a compact stretch");
    }

    struct Half;
    impl crate::story::card_layout::CardMeasure for Half {
        fn width(&self, text: &str, size: f32, _: crate::story::design::TextStyle) -> f32 {
            text.chars().count() as f32 * size * 0.5
        }
    }

    fn portrait() -> crate::story::card_layout::FrameSpec {
        crate::story::card_layout::FrameSpec {
            w: 1080.0,
            h: 1920.0,
            aspect: crate::story::tiers::Aspect::Portrait,
            safe_area: crate::story::tiers::SafeArea::None,
            dir: crate::story::frame_layout::Dir::Ltr,
            anchor: crate::story::frame_layout::AnchorPref::Auto,
            tone: crate::story::design::Tone::Paper,
            mark_w: None,
            credits: crate::story::frame_layout::CreditsPlacement::Bottom,
        }
    }

    /// The layout a frame draws for `a`: eased from its `from_card` while
    /// the ease runs, as the drawer does.
    fn drawn(a: &CardAt) -> crate::story::card_layout::CardLayout {
        use crate::story::card_layout::{ease_cards, layout_card};
        let lay = |c: &CardContent| layout_card(&portrait(), Some(c), &[], None, &Half);
        match a.from_card.as_ref().filter(|_| a.ease < 1.0) {
            Some(from) => ease_cards(&lay(from), &lay(&a.card), a.ease),
            None => lay(&a.card),
        }
    }

    /// Each drawn line's text and alpha (no `run_alpha` is all 1, settled).
    fn lines(l: &crate::story::card_layout::CardLayout) -> Vec<(String, f32)> {
        l.runs.iter().enumerate().map(|(i, r)| (r.text.clone(), l.run_alpha.get(i).copied().unwrap_or(1.0))).collect()
    }

    /// Ruling R6-a: a cue over a card with a body takes row 2; the body
    /// yields while the cue runs and comes back when it ends. The headline
    /// holds through both swaps.
    #[test]
    fn a_cue_over_a_body_card_is_drawn() {
        let card = CardContent { body: Some("The body text".into()), ..headline("The bay") };
        let spans = vec![CardSpan { id: "bay".into(), from: 0.0, to: 10.0, content: card, fade_s: CARD_FADE_S, progress: true }];
        let cues = [cue(3.0, 6.0, "The cue text")];
        let texts = |t: f64| -> Vec<String> {
            lines(&drawn(&card_at(&spans, &cues, &[], t).unwrap())).into_iter().filter(|(_, k)| *k > 0.0).map(|(r, _)| r).collect()
        };
        let during = texts(4.5);
        assert!(during.iter().any(|r| r == "The cue text"), "the cue is drawn: {during:?}");
        assert!(!during.iter().any(|r| r.contains("body")), "the body yields: {during:?}");
        let after = texts(7.0);
        assert!(after.iter().any(|r| r == "The body text") && !after.iter().any(|r| r.contains("cue")), "the body comes back: {after:?}");
        for t in [3.05, 3.15, 3.25, 6.05, 6.15, 6.25] {
            let l = drawn(&card_at(&spans, &cues, &[], t).unwrap());
            let head: f32 = lines(&l).into_iter().filter(|(r, _)| r == "The bay").map(|(_, k)| k).sum();
            assert_eq!(head, 1.0, "at {t}: the headline holds, drawn once");
        }
    }

    /// Ruling R6-b: a cue in a card with a headline is set exactly as the
    /// body it replaces; a caption-only card keeps the caption style.
    #[test]
    fn an_in_card_cue_takes_the_body_style() {
        let style = |l: &crate::story::card_layout::CardLayout, text: &str| {
            let r = l.runs.iter().find(|r| r.text == text).unwrap_or_else(|| panic!("no run {text:?}: {:?}", l.runs));
            (r.size, r.weight, r.line_height, r.ink)
        };
        let lay = |c: &CardContent| crate::story::card_layout::layout_card(&portrait(), Some(c), &[], None, &Half);
        let with_body = lay(&CardContent { body: Some("Short body".into()), ..headline("The bay") });
        let spans = vec![CardSpan { id: "bay".into(), from: 0.0, to: 10.0, content: headline("The bay"), fade_s: CARD_FADE_S, progress: true }];
        let cues = [cue(2.0, 5.0, "Short cue")];
        let in_card = drawn(&card_at(&spans, &cues, &[], 3.0).unwrap());
        assert_eq!(style(&in_card, "Short cue"), style(&with_body, "Short body"), "the cue takes the body's style");
        let caption_only = lay(&CardContent { caption: Some("Short caption".into()), ..CardContent::default() });
        let alone = drawn(&card_at(&[], &cues, &[], 3.0).unwrap());
        assert_eq!(style(&alone, "Short cue"), style(&caption_only, "Short caption"), "a caption-only card keeps the caption style");
        assert_ne!(style(&alone, "Short cue"), style(&in_card, "Short cue"));
    }

    /// Spec §3.2: across a fly's boundaries only the hidden rows fade; the
    /// caption row, shown compact and full, never blinks.
    #[test]
    fn the_caption_row_holds_across_a_camera_boundary() {
        let card = CardContent { caption: Some("Leaving the bay".into()), ..headline("The bay") };
        let spans = vec![CardSpan { id: "bay".into(), from: 0.0, to: 10.0, content: card, fade_s: CARD_FADE_S, progress: true }];
        let moving = [(3.0, 6.0)];
        for t in [2.9, 3.0, 3.05, 3.15, 3.25, 4.5, 6.0, 6.1, 6.2, 6.29, 7.0] {
            let l = drawn(&card_at(&spans, &[], &moving, t).unwrap());
            let shown: Vec<f32> = lines(&l).into_iter().filter(|(r, k)| r == "Leaving the bay" && *k > 0.0).map(|(_, k)| k).collect();
            assert_eq!(shown, vec![1.0], "at {t}: the caption row is drawn once at full alpha");
        }
        // The headline is a hidden row: it fades.
        let l = drawn(&card_at(&spans, &[], &moving, 3.1).unwrap());
        let head: Vec<f32> = lines(&l).into_iter().filter(|(r, _)| r == "The bay").map(|(_, k)| k).collect();
        assert!(head.iter().all(|k| *k < 1.0), "{head:?}");
    }

    /// Spec §3.4, §3.6: a photo's credit joins the sources as `Photo`,
    /// after the author's groups and before the overlays' `Data`; a
    /// repeated group once.
    #[test]
    fn photo_credits_join_the_sources_as_photo() {
        let script = minimal_script(serde_json::json!({"furniture": {"sources": [{"key": "Map data", "text": "OpenStreetMap"}]}}));
        let got = sources_at(&script, &["Jane Doe / PUB".into(), " ".into(), "Jane Doe / PUB".into()], &["Route: illustrative".into()]);
        let want: Vec<SourceGroup> = [("Map data", "OpenStreetMap"), ("Photo", "Jane Doe / PUB"), ("Data", "Route: illustrative")]
            .into_iter()
            .map(|(k, t)| SourceGroup { key: k.into(), text: t.into() })
            .collect();
        assert_eq!(got, want);
    }

    /// The hairline reads the chapter at `t`, as before the move.
    #[test]
    fn progress_follows_the_chapters() {
        let ch = |from: f64, to: f64| Chapter { id: String::new(), title: String::new(), from, to, alt: None, still_t: from, furniture: None, implicit: false };
        assert_eq!(progress_at(&[ch(0.0, 10.0)], 5.0), None, "no hairline with one chapter");
        let two = [ch(0.0, 4.0), ch(4.0, 10.0)];
        assert_eq!(progress_at(&two, 2.0), Some(0.25));
        assert_eq!(progress_at(&two, 7.0), Some(0.75));
    }
}
