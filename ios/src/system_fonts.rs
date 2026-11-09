//! The phone's own fonts for every script, through CoreText.
//!
//! `fontdb::Database::load_system_fonts` has no iOS branch (iOS is `unix`
//! and not `macos`, so it scans Linux directories and finds nothing; see
//! `lib.rs` on `font_fallback`), and the shell bundles one Latin subset. So
//! a Japanese, Thai or Arabic stop name shaped against nothing but boxes.
//!
//! This asks CoreText instead, for exactly the families
//! `font_fallback::script_fallback_families` names for each script (Hiragino
//! and PingFang for CJK, Geeza Pro for Arabic, Thonburi for Thai, Kohinoor
//! for the Indic scripts...): a descriptor per family name, matched, and its
//! `kCTFontURLAttribute` -- the face's file -- loaded into fontdb, which
//! memory-maps it. The fallback chain then finds a face per script the way
//! it does on the desktop. Only those families are touched, not the whole
//! font book: a few dozen descriptor lookups and file maps, measured by the
//! renderer's build timings (`labels (fonts, text atlas)`).
//!
//! A family CoreText knows but whose file cannot be read (a font the system
//! serves on demand) is reported missing rather than rebuilt from its
//! tables; the report says which, per script.

use crate::font_fallback::script_fallback_families;
pub use glyphon::fontdb::Database;
pub use unicode_script::Script;

/// The scripts a map label is drawn in that the bundled Latin face does not
/// cover, each given its own fallback family from the system.
pub const SCRIPTS: &[Script] = &[
    Script::Han,
    Script::Hiragana,
    Script::Katakana,
    Script::Hangul,
    Script::Bopomofo,
    Script::Arabic,
    Script::Hebrew,
    Script::Thai,
    Script::Lao,
    Script::Khmer,
    Script::Myanmar,
    Script::Devanagari,
    Script::Bengali,
    Script::Gujarati,
    Script::Gurmukhi,
    Script::Kannada,
    Script::Malayalam,
    Script::Oriya,
    Script::Sinhala,
    Script::Tamil,
    Script::Telugu,
    Script::Tibetan,
    Script::Armenian,
    Script::Georgian,
    Script::Ethiopic,
];

/// Faces for the scripts above, and for the Latin/Greek/Cyrillic text the
/// bundled subset does not reach (Vietnamese diacritics, Cyrillic): the
/// system UI family's file.
const LATIN_FAMILIES: &[&str] = &["Helvetica Neue", "Helvetica", "Arial"];

/// What [`load`] found.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Files loaded (a `.ttc` collection counts once).
    pub files: usize,
    /// Faces fontdb holds afterwards that came from those files.
    pub faces: usize,
    /// Scripts none of whose families resolved to a readable file.
    pub missing_scripts: Vec<Script>,
    pub millis: f64,
}

/// Loads the system faces the label fallback chain names for every script in
/// [`SCRIPTS`] (and a Latin UI family) into `db`.
pub fn load(db: &mut glyphon::fontdb::Database, locale: &str) -> Report {
    let started = std::time::Instant::now();
    let faces_before = db.len();
    let mut report = Report::default();
    let mut loaded: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    let mut load_family = |family: &str, report: &mut Report| -> bool {
        let Some(path) = family_file(family) else { return false };
        if loaded.contains(&path) {
            return true;
        }
        match db.load_font_file(&path) {
            Ok(()) => {
                report.files += 1;
                loaded.insert(path);
                true
            }
            Err(_) => false,
        }
    };
    for family in LATIN_FAMILIES {
        if load_family(family, &mut report) {
            break;
        }
    }
    for &script in SCRIPTS {
        let mut any = false;
        for family in script_fallback_families(script, locale) {
            // Every family that exists, not only the first: the fallback
            // walks them in order per codepoint, and a later one covers what
            // an earlier one lacks.
            any |= load_family(family, &mut report);
        }
        if !any {
            report.missing_scripts.push(script);
        }
    }
    report.faces = db.len() - faces_before;
    report.millis = started.elapsed().as_secs_f64() * 1000.0;
    report
}

/// The file of the face CoreText matches for `family`, when it is that
/// family and the file is readable.
#[cfg(target_vendor = "apple")]
pub fn family_file(family: &str) -> Option<std::path::PathBuf> {
    use objc2_core_foundation::{CFDictionary, CFString, CFType, CFURL};
    use objc2_core_text::{kCTFontFamilyNameAttribute, kCTFontURLAttribute, CTFontDescriptor};

    let name = CFString::from_str(family);
    // SAFETY: CoreText's documented attribute keys, with a CFString value
    // as `kCTFontFamilyNameAttribute` requires.
    let (key, url_key) = unsafe { (kCTFontFamilyNameAttribute, kCTFontURLAttribute) };
    let attributes = CFDictionary::<CFString, CFString>::from_slices(&[key], &[&name]);
    // SAFETY: a dictionary of descriptor attributes, as the call requires.
    let descriptor = unsafe { CTFontDescriptor::with_attributes(attributes.as_opaque()) };
    // SAFETY: no mandatory attributes; matching may fall back to another
    // family, which the family check below rejects.
    let matched = unsafe { descriptor.matching_font_descriptor(None) }?;
    // SAFETY: documented attribute keys on a live descriptor.
    let got_family = unsafe { matched.attribute(key) }?.downcast::<CFString>().ok()?.to_string();
    if !got_family.eq_ignore_ascii_case(family) {
        return None;
    }
    let url: objc2_core_foundation::CFRetained<CFType> = unsafe { matched.attribute(url_key) }?;
    let path = url.downcast::<CFURL>().ok()?.to_file_path()?;
    std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
    Some(path)
}

#[cfg(not(target_vendor = "apple"))]
pub fn family_file(_family: &str) -> Option<std::path::PathBuf> {
    None
}

/// One shaped glyph: its id in its face, the byte range of the text it
/// came from, where it sits on the line, and whether it is in an RTL run.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: u16,
    pub start: usize,
    pub end: usize,
    pub x: f32,
    pub rtl: bool,
    pub family: String,
}

/// `text` shaped the way a label is -- the bundled face first, the system
/// fallback per script ([`load`]), cosmic-text's shaping and bidi -- on one
/// line. For tests, and for checking a device's font coverage.
#[doc(hidden)]
pub fn shape_label(text: &str) -> Vec<ShapedGlyph> {
    let mut db = glyphon::fontdb::Database::new();
    db.load_font_data(crate::app::EMBEDDED_FONT.to_vec());
    load(&mut db, "en-US");
    let mut font_system = crate::font_fallback::new_font_system_from_db("en-US".to_string(), db);
    let mut buffer = glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(16.0, 20.0));
    let attrs = glyphon::Attrs::new().family(glyphon::Family::Name(crate::app::FONT_FAMILY));
    buffer.set_text(text, &attrs, glyphon::Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut font_system, false);
    let mut out = Vec::new();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let family = font_system
                .db()
                .face(glyph.font_id)
                .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
                .unwrap_or_default();
            out.push(ShapedGlyph {
                glyph_id: glyph.glyph_id,
                start: glyph.start,
                end: glyph.end,
                x: glyph.x,
                rtl: glyph.level.is_rtl(),
                family,
            });
        }
    }
    out
}
