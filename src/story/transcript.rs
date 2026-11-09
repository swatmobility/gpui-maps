//! `transcript.html`, the bundle's "read as article" page (spec §5).
//!
//! Plain semantic HTML: the headline and summary, then every chapter in
//! time order -- its still (the chapter's alt text on the `<img>` only;
//! a `<figcaption>` is only ever an image's credit), each panel's text
//! in reading order (headline, body, source), each callout's text, the
//! captions shown during it, and each image with its alt text and credit.
//! No script, no inline event handler, no external resource: the page
//! reads the bundle's own `stills/` and `media/` by relative path, styles
//! itself with one inline `<style>` block (system fonts, a viewport meta,
//! readable on a phone), and works from a file:// URL or any static host.
//!
//! Every author string passes [`redact_url_secrets`] (no credential lands
//! in a published file) and is then HTML-escaped, attribute or text alike:
//! a caption reading `<script>` is shown as those characters.
//!
//! Pure (no I/O). Native-only only because it reads [`LoadedImage`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use super::captions::Cue;
use super::chapters::Chapter;
use super::media::LoadedImage;
use super::redact_url_secrets;
use super::script::ImageSpec;
use super::timeline::ElementKind;

/// The page's whole stylesheet: system fonts, a readable measure, images
/// that never overflow a phone screen, and a dark scheme that follows the
/// reader's setting. No `url(...)`, no `@import`, no web font.
const STYLE: &str = "\
:root{color-scheme:light dark;--fg:#1b1b1b;--muted:#5c5c5c;--bg:#fdfdfc;--rule:#d9d9d6}\
@media (prefers-color-scheme:dark){:root{--fg:#ececea;--muted:#a6a6a2;--bg:#141414;--rule:#3a3a38}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);font:1.0625rem/1.6 system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif}\
article{max-width:42rem;margin:0 auto;padding:1.5rem 1rem 3rem}\
h1{font-size:1.75rem;line-height:1.25;margin:0 0 .75rem}\
h2{font-size:1.3rem;line-height:1.3;margin:2.25rem 0 .75rem;padding-top:1.25rem;border-top:1px solid var(--rule)}\
h3{font-size:1.05rem;margin:1.25rem 0 .25rem}\
p{margin:.5rem 0;overflow-wrap:anywhere}\
.summary{color:var(--muted);font-size:1.1rem}\
figure{margin:1rem 0}\
img{display:block;max-width:100%;height:auto;border-radius:4px}\
figcaption,small{color:var(--muted);font-size:.875rem}\
figcaption{margin-top:.35rem}\
.captions{margin:1rem 0 0;padding-left:.9rem;border-left:3px solid var(--rule)}\
.captions p{margin:.25rem 0}";

/// `s` with credentials redacted ([`redact_url_secrets`]), then escaped
/// for HTML text or a double- or single-quoted attribute value.
#[must_use]
pub fn escape(s: &str) -> String {
    let clean = redact_url_secrets(s);
    let mut out = String::with_capacity(clean.len() + 8);
    for c in clean.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The file stem each chapter's still is written under (`stills/<stem>.png`)
/// and its `<section>` id, aligned with `chapters`: the id with every
/// character outside `[A-Za-z0-9_-]` turned into `-` and dashes trimmed from
/// both ends (so no separator, no `..`, no dot-file, no leading dash),
/// `chapter` when nothing is left, and `-2`, `-3`, ... appended to any stem an earlier
/// chapter already took -- compared case-insensitively, since macOS and
/// Windows file systems would otherwise write `One` and `one` to one file.
#[must_use]
pub fn still_stems(chapters: &[Chapter]) -> Vec<String> {
    let mut taken: Vec<String> = Vec::with_capacity(chapters.len());
    let mut out = Vec::with_capacity(chapters.len());
    for chapter in chapters {
        let base: String =
            chapter.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
        // No leading dash either: a file named `-x.png` reads as a flag to a shell tool.
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "chapter".to_string() } else { base.to_string() };
        let mut stem = base.clone();
        let mut n = 2;
        while taken.contains(&stem.to_ascii_lowercase()) {
            stem = format!("{base}-{n}");
            n += 1;
        }
        taken.push(stem.to_ascii_lowercase());
        out.push(stem);
    }
    out
}

/// The indices of `chapters` in time order (by `from`; document order at
/// equal times): the order the transcript reads them in and the "first
/// chapter" whose still is the poster.
#[must_use]
pub fn time_order(chapters: &[Chapter]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..chapters.len()).collect();
    order.sort_by(|a, b| {
        let from = |i: &usize| chapters.get(*i).map_or(0.0, |c| c.from);
        from(a).total_cmp(&from(b))
    });
    order
}

/// Which of `order` (indices into `chapters`, time order) an item at `t`
/// belongs to, as a position in `order`: the chapter containing `t`
/// ([`super::chapter_at`]); in a gap, the last chapter that started
/// before `t`; before every chapter, the first. Nothing the story shows is
/// dropped from the transcript for falling between chapters.
fn chapter_slot(chapters: &[Chapter], order: &[usize], t: f64) -> usize {
    if let Some(found) = super::chapter_at(chapters, t) {
        if let Some(slot) = order.iter().position(|i| chapters.get(*i).is_some_and(|c| std::ptr::eq(c, found))) {
            return slot;
        }
    }
    order.iter().rposition(|i| chapters.get(*i).is_some_and(|c| c.from <= t)).unwrap_or(0)
}

/// `<figure>` for an image the story names, when it loaded: the bundle's
/// copy (`media/<bundle_name>`), its alt text, and its credit as the
/// caption. One that did not load leaves its alt text as a paragraph.
fn image_figure(out: &mut String, spec: &ImageSpec, images: &[LoadedImage]) {
    let alt = spec.alt.as_deref().map(str::trim).unwrap_or("");
    let credit = spec.credit.as_deref().map(str::trim).filter(|c| !c.is_empty());
    match images.iter().find(|i| i.src == spec.src) {
        Some(image) => {
            out.push_str(&format!("<figure><img src=\"media/{}\" alt=\"{}\" loading=\"lazy\">", escape(&media_name(image)), escape(alt)));
            if let Some(credit) = credit {
                out.push_str(&format!("<figcaption dir=\"auto\">{}</figcaption>", escape(credit)));
            }
            out.push_str("</figure>\n");
        }
        None if !alt.is_empty() => out.push_str(&format!("<p dir=\"auto\"><em>Image: {}</em></p>\n", escape(alt))),
        None => {}
    }
}

/// The name `image` is copied to under `media/`: the first 16 hex digits
/// of the SHA-256 of its ORIGINAL bytes (computed here, never trusted from
/// a field), plus its extension when that is one of the known image types
/// (`bin` otherwise). Content-addressed, so a hostile `src` or
/// `bundle_name` cannot steer the path: no separator, no `..`, no dot-file.
#[must_use]
pub fn media_name(image: &LoadedImage) -> String {
    use sha2::Digest as _;
    let hex: String = sha2::Sha256::digest(&image.source).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let ext = image.bundle_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let ext = if ["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) { ext } else { "bin".to_string() };
    format!("{hex}.{ext}")
}

/// The transcript page, in `lang` (the script's BCP-47 `lang`; `None`,
/// or a value that is not a tag, is `"en"`). `chapters` as the timeline resolved them (any
/// order; the page reads them by time), `elements` as
/// `Timeline::element_plans` gives them (document order), `cues` the one
/// caption track, `images` every image that loaded. Each chapter's still is
/// `stills/<stem>.png` ([`still_stems`]); each image is `media/<name>`
/// ([`media_name`]).
#[must_use]
pub fn transcript_html(
    title: &str,
    summary: Option<&str>,
    lang: Option<&str>,
    chapters: &[Chapter],
    elements: &[(f64, ElementKind)],
    cues: &[Cue],
    images: &[LoadedImage],
) -> String {
    let order = time_order(chapters);
    let stems = still_stems(chapters);
    let slots = order.len().max(1);
    // Each chapter's text, filled in document order, then written in time order.
    let mut panels: Vec<String> = vec![String::new(); slots];
    let mut figures: Vec<String> = vec![String::new(); slots];
    for (at, kind) in elements {
        let slot = chapter_slot(chapters, &order, *at);
        let (Some(text), Some(figs)) = (panels.get_mut(slot), figures.get_mut(slot)) else { continue };
        match kind {
            ElementKind::Panel(p) => {
                if let Some(h) = p.headline.as_deref().filter(|s| !s.trim().is_empty()) {
                    text.push_str(&format!("<h3 dir=\"auto\">{}</h3>\n", escape(h)));
                }
                if let Some(b) = p.body.as_deref().filter(|s| !s.trim().is_empty()) {
                    text.push_str(&format!("<p dir=\"auto\">{}</p>\n", escape(b)));
                }
                if let Some(s) = p.source.as_deref().filter(|s| !s.trim().is_empty()) {
                    text.push_str(&format!("<p dir=\"auto\"><small>{}</small></p>\n", escape(s)));
                }
                if let Some(spec) = &p.image {
                    image_figure(text, spec, images);
                }
            }
            ElementKind::Callout(c) => {
                if !c.text.trim().is_empty() {
                    text.push_str(&format!("<p dir=\"auto\">{}</p>\n", escape(&c.text)));
                }
                if let Some(spec) = &c.image {
                    image_figure(text, spec, images);
                }
            }
            ElementKind::Image(spec) => image_figure(figs, spec, images),
        }
    }
    let mut captions: Vec<String> = vec![String::new(); slots];
    for cue in cues {
        if let Some(c) = captions.get_mut(chapter_slot(chapters, &order, cue.from)) {
            c.push_str(&format!("<p dir=\"auto\">{}</p>\n", escape(&cue.text)));
        }
    }

    let title = if title.trim().is_empty() { "Story" } else { title };
    let mut out = String::new();
    let lang = lang.filter(|l| super::script::is_bcp47_like(l)).unwrap_or("en");
    out.push_str(&format!("<!doctype html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n", escape(lang)));
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str(&format!("<title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<article>\n", escape(title)));
    out.push_str(&format!("<h1 dir=\"auto\">{}</h1>\n", escape(title)));
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("<p class=\"summary\" dir=\"auto\">{}</p>\n", escape(s)));
    }
    for (slot, index) in order.iter().enumerate() {
        let (Some(chapter), Some(stem)) = (chapters.get(*index), stems.get(*index)) else { continue };
        let alt = chapter.alt.as_deref().map(str::trim).unwrap_or("");
        out.push_str(&format!("<section id=\"{}\">\n<h2 dir=\"auto\">{}</h2>\n", escape(stem), escape(&chapter.title)));
        // The alt text only on the `<img>`: a figcaption repeating it would
        // be read twice by a screen reader. A figcaption is for a credit,
        // and a still of the story's own map has none of its own.
        out.push_str(&format!("<figure><img src=\"stills/{}.png\" alt=\"{}\"></figure>\n", escape(stem), escape(alt)));
        out.push_str(panels.get(slot).map_or("", String::as_str));
        if let Some(c) = captions.get(slot).filter(|c| !c.is_empty()) {
            out.push_str(&format!("<div class=\"captions\">\n{c}</div>\n"));
        }
        out.push_str(figures.get(slot).map_or("", String::as_str));
        out.push_str("</section>\n");
    }
    out.push_str("</article>\n</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn chapter(id: &str, from: f64, to: f64, alt: Option<&str>) -> Chapter {
        Chapter {
            id: id.into(),
            title: format!("Title {id}"),
            from,
            to,
            alt: alt.map(Into::into),
            still_t: (from + to) / 2.0,
            furniture: None,
            implicit: false,
        }
    }

    fn panel(headline: &str) -> ElementKind {
        ElementKind::Panel(serde_json::from_value(serde_json::json!({"placement": "lower-third", "headline": headline, "body": "Body text", "source": "Source: X"})).unwrap())
    }

    #[test]
    fn the_transcript_is_escaped_and_has_no_script() {
        let hostile = "<script>alert(1)</script> & co";
        let chapters = vec![chapter("one", 0.0, 1.0, Some("<img src=x onerror=alert(1)>")), chapter("two", 1.0, 2.0, None)];
        let elements = vec![(0.5, panel(hostile)), (1.5, ElementKind::Callout(serde_json::from_value(serde_json::json!({"at_coord": [0.0, 0.0], "text": "\" onload=\"x"})).unwrap()))];
        let cues = vec![Cue { index: 1, from: 0.2, to: 0.8, text: hostile.into() }];
        let html = transcript_html(hostile, Some("see https://x.example/a?access_token=pk.SECRET"), None, &chapters, &elements, &cues, &[]);
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; co"), "{html}");
        assert!(!html.to_ascii_lowercase().contains("<script"), "{html}");
        // No real tag carries an event handler: an `on...=` the author
        // wrote is escaped text, never inside `<...>`.
        for tag in html.split('<').skip(1).map(|rest| rest.split('>').next().unwrap_or("")) {
            // Outside its quoted values (an escaped `"` never closes one).
            let bare: String = tag.split('"').step_by(2).collect();
            assert!(!bare.to_ascii_lowercase().contains(" on"), "<{tag}> in {html}");
        }
        // The brief's case verbatim: the script headline alone leaves no handler text at all.
        let plain = transcript_html(hostile, None, None, &chapters[1..], &[(1.5, panel(hostile))], &cues, &[]);
        assert!(plain.contains("&lt;script&gt;") && !plain.contains("<script"), "{plain}");
        assert!(!plain.contains("onerror=") && !plain.contains("onload=") && !plain.contains("http"), "{plain}");
        assert!(!html.contains("SECRET"), "credentials are redacted: {html}");
        // `http` only inside escaped author text, never as a resource.
        for (i, _) in html.match_indices("http") {
            let before = &html[..i];
            assert!(!before.ends_with("src=\"") && !before.ends_with("href=\""), "{html}");
        }
        assert!(!html.contains("src=\"http") && !html.contains("href=") && !html.contains("@import") && !html.contains("url("), "{html}");
        assert!(html.contains("<meta name=\"viewport\""), "readable on a phone");
    }

    #[test]
    fn chapters_read_in_time_order_with_their_panels_captions_and_images() {
        // Document order: two before one.
        let chapters = vec![chapter("two", 5.0, 10.0, Some("Second")), chapter("one", 0.0, 5.0, Some("First"))];
        let image = LoadedImage {
            src: "img/a.png".into(),
            image: Arc::new(image::RgbaImage::new(1, 1)),
            source: Arc::from(&b"png bytes"[..]),
            sha256: "00".into(),
            bundle_name: "../../etc/passwd.png".into(),
            original: (1, 1),
            animated: false,
        };
        let img = ElementKind::Image(serde_json::from_value(serde_json::json!({"src": "img/a.png", "alt": "A harbour", "credit": "Photo: Test"})).unwrap());
        // In a gap after `two` ends: still listed, under `two`.
        let elements = vec![(6.0, panel("Later")), (1.0, panel("Earlier")), (2.0, img), (11.0, panel("After the end"))];
        let cues = vec![Cue { index: 1, from: 7.0, to: 8.0, text: "cue in two".into() }];
        let html = transcript_html("T", None, None, &chapters, &elements, &cues, std::slice::from_ref(&image));
        let at = |s: &str| html.find(s).unwrap_or_else(|| panic!("{s} missing: {html}"));
        assert!(at("<section id=\"one\">") < at("Earlier") && at("Earlier") < at("<section id=\"two\">"));
        // Within a chapter: panel text (document order), then its captions.
        assert!(at("<section id=\"two\">") < at("Later") && at("Later") < at("After the end") && at("After the end") < at("cue in two"));
        assert!(html.contains("<img src=\"stills/one.png\" alt=\"First\">"), "{html}");
        let name = media_name(&image);
        assert!(!name.contains('/') && !name.contains("..") && name.ends_with(".png") && name.len() == 20, "{name}");
        assert!(html.contains(&format!("<img src=\"media/{name}\" alt=\"A harbour\"")), "{html}");
        assert!(html.contains("<figcaption dir=\"auto\">Photo: Test</figcaption>"), "credited: {html}");
        // Reading order inside a panel: headline, body, source.
        assert!(at("Earlier") < html[at("Earlier")..].find("Body text").unwrap() + at("Earlier"));
    }

    /// Controller ruling, fix round 1: a chapter still carries its alt text
    /// on the `<img>` only -- no figcaption repeats it -- while an image's
    /// figcaption is its credit and never its alt.
    #[test]
    fn a_figcaption_never_repeats_the_alt_text() {
        let chapters = vec![chapter("one", 0.0, 1.0, Some("The camera arrives."))];
        let img = ElementKind::Image(serde_json::from_value(serde_json::json!({"src": "img/a.png", "alt": "A harbour", "credit": "Photo: Test"})).unwrap());
        let image = LoadedImage {
            src: "img/a.png".into(),
            image: Arc::new(image::RgbaImage::new(1, 1)),
            source: Arc::from(&b"x"[..]),
            sha256: String::new(),
            bundle_name: "a.png".into(),
            original: (1, 1),
            animated: false,
        };
        let html = transcript_html("T", None, None, &chapters, &[(0.5, img)], &[], std::slice::from_ref(&image));
        assert!(html.contains("<figure><img src=\"stills/one.png\" alt=\"The camera arrives.\"></figure>"), "{html}");
        assert_eq!(html.matches("The camera arrives.").count(), 1, "the alt text once: {html}");
        assert_eq!(html.matches("<figcaption").count(), 1, "one figcaption, the credit's: {html}");
        assert!(html.contains("<figcaption dir=\"auto\">Photo: Test</figcaption>") && html.matches("A harbour").count() == 1, "{html}");
    }

    /// Controller ruling, fix round 1: `<html lang>` is the script's `lang`,
    /// `en` when it has none (or one that is not a tag).
    #[test]
    fn the_page_language_is_the_scripts_lang() {
        let chapters = vec![chapter("one", 0.0, 1.0, None)];
        let page = |lang: Option<&str>| transcript_html("T", None, lang, &chapters, &[], &[], &[]);
        assert!(page(Some("ja")).starts_with("<!doctype html>\n<html lang=\"ja\">"));
        assert!(page(Some("pt-BR")).contains("<html lang=\"pt-BR\">"));
        assert!(page(None).contains("<html lang=\"en\">"));
        assert!(page(Some("\"><script>")).contains("<html lang=\"en\">"), "a non-tag never reaches the attribute");
    }

    #[test]
    fn still_stems_are_safe_and_distinct_ignoring_case() {
        let chapters = vec![chapter("One", 0.0, 1.0, None), chapter("one", 1.0, 2.0, None), chapter("../x", 2.0, 3.0, None), chapter("...", 3.0, 4.0, None), chapter("a b/c", 4.0, 5.0, None)];
        assert_eq!(still_stems(&chapters), vec!["One", "one-2", "x", "chapter", "a-b-c"]);
    }
}
