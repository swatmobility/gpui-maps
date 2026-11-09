//! Which font draws a glyph the label's own face cannot.
//!
//! # What this is for
//!
//! `label_text_attrs` asks for one family by name -- [`DEFAULT_LABEL_FAMILY`],
//! `SF Pro Text` on macOS (DejaVu Sans on Linux, Segoe UI on Windows) -- and
//! that family covers Latin, Greek and Cyrillic and essentially nothing
//! else. Every other script on the map (Han, Kana, Hangul, Arabic,
//! Devanagari, Thai, ...) is drawn by some OTHER face, chosen at shape time
//! per word. cosmic-text does the choosing; this module tells it what to
//! choose from.
//!
//! cosmic-text ships a per-platform table of its own
//! (`cosmic_text::PlatformFallback`) and picks the table by `cfg`. That is
//! the problem this module exists to fix: the table it picks is chosen by
//! the compile target, not by what is actually installed. On macOS the list
//! names macOS faces and works. On iOS -- `unix`, not `macos` -- cosmic-text
//! selects its *Unix* list, which asks for `Noto Sans CJK JP`, `DejaVu
//! Sans` and `FreeSans`; iOS has none of them and ships `Hiragino Sans` and
//! `PingFang SC` instead. On `wasm32` the list is empty outright. In both
//! cases the per-script stage finds nothing and shaping falls through to
//! cosmic-text's last resort, an exhaustive walk of every face in the
//! database in database order -- which draws *something*, from whichever
//! face happens to sort first, inconsistently between words.
//!
//! So the fallback list here is a UNION across platforms rather than one
//! platform's. Each script names the macOS/iOS faces, then the Windows
//! ones, then the Noto/Linux ones. cosmic-text skips a family that is not
//! in the database, so naming a face no platform has costs nothing but a
//! string; naming one the machine does have is the difference between a
//! script rendering and not.
//!
//! # Why this cannot regress Latin
//!
//! `script_fallback_families` returns an EMPTY list for Latin, Greek and
//! Cyrillic. Those scripts never reach the fallback machinery at all --
//! `SF Pro Text` covers them, so shaping is satisfied by the requested
//! family and stops. The lists below are consulted only for a codepoint the
//! requested family cannot draw.
//!
//! And for every other script, `COMMON_FALLBACK` and each script list open
//! with cosmic-text's own macOS entries, in cosmic-text's own order. On
//! macOS the appended cross-platform names are all absent from the font
//! database, so they are skipped and the resolved face is byte-identical to
//! what cosmic-text resolved before this module existed.
//!
//! # Where it still gives up
//!
//! If no listed family is installed and no face in the database covers the
//! codepoint, cosmic-text returns no font and the glyph is simply not
//! drawn. That is the intended floor: a missing glyph is a missing glyph,
//! not a panic and not a tofu box. Nothing here indexes a table by
//! codepoint, so an unassigned or unknown codepoint cannot be a lookup
//! failure either -- `unicode_script` maps it to `Script::Unknown` and
//! `script_fallback_families` answers with an empty list.

use unicode_script::Script;

/// Builds the engine's `FontSystem`, with [`MapLabelFallback`] installed in
/// place of cosmic-text's compile-target-chosen table.
///
/// This is `FontSystem::new()` reproduced -- system fonts loaded, the same
/// three default families set, the system locale read -- with the one
/// substitution, because cosmic-text offers no way to swap the fallback on
/// an already-built `FontSystem`.
///
/// The locale matters and is not decoration: it is the only input Han
/// unification has (see [`han_unification`]). A machine reporting `ja-JP`
/// gets Japanese glyph forms for the shared CJK ideographs; one reporting
/// `zh-TW` gets traditional forms. Reading it here rather than hardcoding
/// one is what keeps this from being a Japan special case.
pub fn new_font_system() -> glyphon::FontSystem {
    let locale = sys_locale::get_locale().unwrap_or_else(|| String::from("en-US"));
    let mut db = glyphon::fontdb::Database::new();
    db.load_system_fonts();
    // The story design system's card faces (spec §4.1, D5), loaded here so
    // both the capture session's and the live map's `FontSystem` carry
    // them. See `load_story_fonts`.
    load_story_fonts(&mut db);
    new_font_system_from_db(locale, db)
}

/// Loads the story design system's card faces (spec §4.1, D5) into `db`.
/// Native only (ruling P6): a no-op on iOS and wasm32, where
/// [`STORY_FONTS`] does not exist, so every caller can call this
/// unconditionally rather than repeating a `cfg`-gated loop. [`new_font_system`]
/// (above) and `capture::session::fonts_for`'s `label_fonts`-override
/// branch both call it -- one `FontSystem` path loading the faces while
/// another silently did not is exactly the bug R1's own golden re-bless
/// caught (the first `new r1` proof run came back `differing=0` because
/// the override branch never had them); this function is the fix for
/// that CLASS of bug, not just that one instance.
#[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
pub(crate) fn load_story_fonts(db: &mut glyphon::fontdb::Database) {
    for bytes in STORY_FONTS {
        db.load_font_data(bytes.to_vec());
    }
}

/// [`load_story_fonts`] on iOS and wasm32, where [`STORY_FONTS`] does not
/// exist (ruling P6): a no-op, so a caller never needs its own `cfg`.
#[cfg(any(target_os = "ios", target_arch = "wasm32"))]
pub(crate) fn load_story_fonts(_db: &mut glyphon::fontdb::Database) {}

/// [`new_font_system`] for a machine that has no font book to enumerate.
///
/// # Why this exists
///
/// `db.load_system_fonts()` is not portable, and -- worse -- it does not
/// FAIL where it is not portable. `fontdb` gates its directory scan on
/// `target_os`, and on `wasm32-unknown-unknown` no arm matches: the call
/// compiles, returns, and leaves the database empty. Every label then
/// shapes against nothing and draws nothing, with no error anywhere. That
/// is exactly the shape of bug this crate's own notes warn about -- a
/// module that compiles clean and renders nothing.
///
/// So the browser build never calls [`new_font_system`]. It loads the font
/// bytes it shipped (`db.load_font_data`), reads its locale off
/// `navigator.language` rather than off a system call, and hands both here.
/// Everything downstream -- the three default families, [`MapLabelFallback`]
/// in place of cosmic-text's compile-target-chosen table -- is the native
/// path's, unchanged, because the difference between the two builds is
/// where the faces came from and nothing else.
pub fn new_font_system_from_db(locale: String, mut db: glyphon::fontdb::Database) -> glyphon::FontSystem {
    // cosmic-text's own three defaults, kept verbatim: this function
    // replaces the fallback table and nothing else.
    db.set_monospace_family("Noto Sans Mono");
    db.set_sans_serif_family("Open Sans");
    db.set_serif_family("DejaVu Serif");
    glyphon::FontSystem::new_with_locale_and_db_and_fallback(locale, db, MapLabelFallback)
}

/// The cross-platform fallback table. See the module doc comment.
#[derive(Debug)]
pub struct MapLabelFallback;

impl glyphon::cosmic_text::Fallback for MapLabelFallback {
    fn common_fallback(&self) -> &[&'static str] {
        COMMON_FALLBACK
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        FORBIDDEN_FALLBACK
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        script_fallback_families(script, locale)
    }
}

/// Tried after every script-specific list, for a codepoint no script list
/// covered -- symbols, emoji, punctuation the requested family lacks.
///
/// The first five are cosmic-text's macOS list, in its order, so macOS
/// resolution is unchanged. `Arial Unicode MS` earns its place at the end
/// of that run for the same reason cosmic-text put it there: it is the
/// broadest single face macOS ships. The rest name the equivalent breadth
/// on Windows and on Linux/web-served font sets.
const COMMON_FALLBACK: &[&str] = &[
    // macOS / iOS
    ".SF NS",
    "Menlo",
    "Apple Color Emoji",
    "Geneva",
    "Arial Unicode MS",
    // Windows
    "Segoe UI",
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    // Linux, and the usual web-served set
    "Noto Sans",
    "Noto Sans Symbols 2",
    "Noto Color Emoji",
    "DejaVu Sans",
    "FreeSans",
];

/// Never fall back to these.
///
/// `.LastResort` is macOS's tofu font: it has a glyph for every codepoint,
/// all of them boxes. Reaching it means shaping "succeeds" and the map
/// draws a row of squares, which is strictly worse than drawing nothing --
/// a blank is legible as missing data, a box row is not.
const FORBIDDEN_FALLBACK: &[&str] = &[".LastResort"];

/// The families to try, in order, for a codepoint of `script` that the
/// requested family cannot draw.
///
/// Empty for Latin, Greek and Cyrillic (the requested family covers them --
/// see the module doc comment) and empty for any script this table does not
/// name, including `Script::Unknown`. An empty answer is not a failure: it
/// hands the codepoint to `COMMON_FALLBACK` and then to cosmic-text's
/// exhaustive database walk, which is exactly the right order of
/// preference.
pub fn script_fallback_families(script: Script, locale: &str) -> &'static [&'static str] {
    match script {
        // Covered by the requested family. Listing anything here would let
        // a Latin label be drawn by a fallback face, which is the one
        // regression this module must not cause.
        Script::Latin | Script::Greek | Script::Cyrillic => &[],

        // CJK. The three Japanese/Korean scripts pin their language
        // outright -- kana is only ever Japanese, Hangul only ever Korean --
        // so they name a face directly instead of asking the locale. Only
        // Han is genuinely ambiguous, and only Han consults the locale.
        Script::Han => han_unification(locale),
        Script::Hiragana | Script::Katakana => JAPANESE,
        Script::Hangul => KOREAN,
        Script::Bopomofo => CHINESE_TRADITIONAL,

        Script::Arabic => &[
            "Geeza Pro",       // macOS / iOS
            "Damascus",        // macOS / iOS
            "Al Nile",         // iOS
            "Segoe UI",        // Windows
            "Tahoma",          // Windows
            "Noto Sans Arabic",
            "Noto Naskh Arabic",
        ],
        Script::Hebrew => &["Arial", "Arial Hebrew", "Segoe UI", "Noto Sans Hebrew"],
        Script::Thaana => &["Noto Sans Thaana", "MV Boli"],
        Script::Syriac => &["Noto Sans Syriac", "Estrangelo Edessa"],

        Script::Devanagari => &[
            "Devanagari Sangam MN",
            "Kohinoor Devanagari",
            "Nirmala UI",
            "Mangal",
            "Noto Sans Devanagari",
        ],
        Script::Bengali => &["Bangla Sangam MN", "Kohinoor Bangla", "Nirmala UI", "Noto Sans Bengali"],
        Script::Gujarati => &["Gujarati Sangam MN", "Kohinoor Gujarati", "Nirmala UI", "Noto Sans Gujarati"],
        Script::Gurmukhi => &["Gurmukhi Sangam MN", "Kohinoor Gurmukhi", "Nirmala UI", "Noto Sans Gurmukhi"],
        Script::Kannada => &["Noto Sans Kannada", "Kannada Sangam MN", "Nirmala UI", "Tunga"],
        Script::Malayalam => &["Malayalam Sangam MN", "Kohinoor Malayalam", "Nirmala UI", "Noto Sans Malayalam"],
        Script::Oriya => &["Noto Sans Oriya", "Oriya Sangam MN", "Nirmala UI", "Kalinga"],
        Script::Sinhala => &["Sinhala Sangam MN", "Nirmala UI", "Iskoola Pota", "Noto Sans Sinhala"],
        Script::Tamil => &["InaiMathi", "Tamil Sangam MN", "Nirmala UI", "Latha", "Noto Sans Tamil"],
        Script::Telugu => &["Telugu Sangam MN", "Kohinoor Telugu", "Nirmala UI", "Gautami", "Noto Sans Telugu"],
        Script::Grantha => &["Grantha Sangam MN", "Noto Sans Grantha"],
        Script::Chakma => &["Noto Sans Chakma"],

        Script::Thai => &["Ayuthaya", "Thonburi", "Leelawadee UI", "Noto Sans Thai"],
        Script::Lao => &["Lao Sangam MN", "Leelawadee UI", "DokChampa", "Noto Sans Lao"],
        Script::Khmer => &["Khmer Sangam MN", "Leelawadee UI", "Khmer UI", "Noto Sans Khmer"],
        Script::Myanmar => &["Noto Sans Myanmar", "Myanmar Sangam MN", "Myanmar Text"],
        Script::Tibetan => &["Kailasa", "Microsoft Himalaya", "Noto Serif Tibetan"],
        Script::Mongolian => &["Noto Sans Mongolian", "Mongolian Baiti"],
        Script::Javanese => &["Noto Sans Javanese"],
        Script::Tai_Le => &["Noto Sans Tai Le", "Microsoft Tai Le"],
        Script::Tai_Tham => &["Noto Sans Tai Tham"],
        Script::Tai_Viet => &["Noto Sans Tai Viet"],
        Script::Buhid => &["Noto Sans Buhid"],
        Script::Hanunoo => &["Noto Sans Hanunoo"],
        Script::Tagalog => &["Noto Sans Tagalog"],
        Script::Tagbanwa => &["Noto Sans Tagbanwa"],
        Script::Yi => &["Noto Sans Yi", "Microsoft Yi Baiti", "PingFang SC"],

        Script::Armenian => &["Noto Sans Armenian", "Mshtakan", "Sylfaen"],
        Script::Georgian => &["Noto Sans Georgian", "Sylfaen"],
        Script::Ethiopic => &["Kefa", "Nyala", "Noto Sans Ethiopic"],
        Script::Cherokee => &["Galvji", "Plantagenet Cherokee", "Noto Sans Cherokee"],
        Script::Canadian_Aboriginal => &["Euphemia UCAS", "Euphemia", "Noto Sans Canadian Aboriginal"],
        Script::Adlam => &["Noto Sans Adlam", "Ebrima"],
        Script::Tifinagh => &["Noto Sans Tifinagh", "Ebrima"],
        Script::Vai => &["Noto Sans Vai", "Ebrima"],
        Script::Gothic => &["Noto Sans Gothic"],

        // Not a guess. See this function's doc comment: an empty list is a
        // handoff to the common list and then to the exhaustive walk, both
        // of which are better-informed than a name picked at random.
        _ => &[],
    }
}

/// Which regional face draws the Han ideographs, which are shared between
/// Chinese, Japanese and Korean but drawn differently in each.
///
/// There is no way to answer this from the codepoints -- that is what "Han
/// unification" means -- so the system locale decides, which is what every
/// other renderer does too.
///
/// Matched on the language subtag with a PREFIX test, not on the whole tag.
/// cosmic-text's own macOS table compares the locale to the literals `"ja"`,
/// `"ko"`, `"zh-HK"` and `"zh-TW"`; a real system reports `ja-JP`, `ko-KR`,
/// `zh-Hant-TW`, so on an actual Japanese Mac every one of those arms misses
/// and Japanese place names are drawn in a Simplified Chinese face. Prefix
/// matching is the whole of the fix.
fn han_unification(locale: &str) -> &'static [&'static str] {
    // ASCII-lowercased so `zh-hant` and `zh-Hant` are the same tag. BCP 47
    // is case-insensitive; only convention capitalises the script subtag.
    let tag = locale.to_ascii_lowercase();
    let is = |prefix: &str| tag == prefix || tag.starts_with(&format!("{prefix}-"));
    if is("ja") {
        JAPANESE
    } else if is("ko") {
        KOREAN
    } else if is("zh-hk") || is("zh-mo") || tag.starts_with("zh-hant-hk") {
        CHINESE_HONGKONG
    } else if is("zh-tw") || tag.starts_with("zh-hant") {
        CHINESE_TRADITIONAL
    } else {
        // Simplified is the default, and catches `zh-CN`, `zh-Hans-*`, and
        // every non-CJK locale -- an `en-GB` machine showing a map of
        // Shanghai wants Simplified, and one showing a map of Tokyo gets
        // its kana from `Script::Hiragana` regardless.
        CHINESE_SIMPLIFIED
    }
}

// Each list opens with the macOS/iOS face cosmic-text's macOS table already
// named, so nothing about macOS resolution moves, then continues with the
// same script's faces on iOS, Windows, and Linux/Noto.
const JAPANESE: &[&str] = &[
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Yu Gothic",
    "Meiryo",
    "MS Gothic",
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "Source Han Sans JP",
];

const KOREAN: &[&str] = &[
    "Apple SD Gothic Neo",
    "AppleGothic",
    "Malgun Gothic",
    "Gulim",
    "Noto Sans CJK KR",
    "Noto Sans KR",
    "Source Han Sans KR",
];

const CHINESE_SIMPLIFIED: &[&str] = &[
    "PingFang SC",
    "Heiti SC",
    "Hiragino Sans GB",
    "Microsoft YaHei",
    "SimSun",
    "Noto Sans CJK SC",
    "Noto Sans SC",
    "Source Han Sans SC",
];

const CHINESE_TRADITIONAL: &[&str] = &[
    "PingFang TC",
    "Heiti TC",
    "Microsoft JhengHei",
    "PMingLiU",
    "Noto Sans CJK TC",
    "Noto Sans TC",
    "Source Han Sans TC",
];

const CHINESE_HONGKONG: &[&str] = &[
    "PingFang HK",
    "Microsoft JhengHei",
    "Noto Sans CJK HK",
    "Noto Sans HK",
    // Traditional forms are the right degradation for Hong Kong; Simplified
    // would be a different writing system, not a different font.
    "PingFang TC",
    "Noto Sans TC",
];

// ---------------------------------------------------------------------------
// Which face a style's `text-font` asks for
// ---------------------------------------------------------------------------

/// The face used when a style names no `text-font` this machine can honour,
/// and the face the engine's own chrome (the attribution credit) is set in.
///
/// **SF Pro Text**, macOS's own UI face, asked for by name. `Family::SansSerif`
/// resolved to Helvetica on this machine -- a 1957 typeface that is not what
/// any other text on the screen is set in, and the single largest reason map
/// labels did not look native beside them. SF Pro *Text*, not *Display*:
/// Apple's optical sizes split at 20pt, and map labels live at 10-14, where
/// Text's looser spacing and larger apertures are what it is drawn for.
///
/// Enumerated from `fontdb` on macOS 15 rather than guessed -- the database
/// carries both the public `SF Pro Text` name and the private `.SF NS`, and
/// the public one is the one to ask for.
///
/// # Off macOS
///
/// Each platform names a face it is likely to have, because a family that
/// is not installed does NOT fall back to anything stable: cosmic-text walks
/// the font database and takes whichever face comes first, and that order is
/// the filesystem's directory order. On Linux, "SF Pro Text" set the
/// credit and the overlay labels in DejaVu Sans on one machine and in Noto
/// Sans Symbols 2 or Noto Naskh Arabic on another; Bitbucket runners
/// differed run to run, and the label-centring test failed by 4.6 px on one.
///
/// * **Linux: DejaVu Sans.** On Debian and Ubuntu, fontconfig's own
///   configuration package depends on it (or one of a few alternatives), so
///   it is the face a Linux desktop is most likely to have. It has no Medium,
///   so the chrome's weight 500 draws as Book (400).
/// * **Windows: Segoe UI**, the system UI face, as SF Pro Text is on macOS.
///
/// A machine without the named face is back to the database walk; this is a
/// preference, not a requirement.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub const DEFAULT_LABEL_FAMILY: &str = "SF Pro Text";
/// See the macOS constant's doc comment, "Off macOS".
#[cfg(target_os = "linux")]
pub const DEFAULT_LABEL_FAMILY: &str = "DejaVu Sans";
/// See the macOS constant's doc comment, "Off macOS".
#[cfg(target_os = "windows")]
pub const DEFAULT_LABEL_FAMILY: &str = "Segoe UI";

/// The style spec's own default for `layout.text-font`, used verbatim for a
/// symbol layer that declares none.
pub const DEFAULT_FONT_STACK: &[&str] = &["Open Sans Regular", "Arial Unicode MS Regular"];

/// A style layer's `text-font`: the face names it asks for, in preference
/// order, exactly as the document wrote them.
///
/// `Arc`-shared because a compiled layer's stack is copied onto every one of
/// the (thousands of) `SymbolFeature`s it bakes, and it is the same three
/// short strings every time.
pub type FontStack = std::sync::Arc<[String]>;

/// [`DEFAULT_FONT_STACK`] as a [`FontStack`] -- what a symbol layer that
/// declares no `text-font` carries, so nothing downstream has an absent case.
pub fn default_font_stack() -> FontStack {
    DEFAULT_FONT_STACK.iter().map(|name| name.to_string()).collect()
}

/// Families a Mapbox font stack names for CODEPOINT COVERAGE rather than for
/// design, and which are therefore skipped when resolving which face a label
/// is set in.
///
/// Mapbox's font stacks are per-codepoint fallback chains, assembled server
/// side: the glyph service takes each codepoint from the first face in the
/// stack that has it. Every Mapbox-authored style ends its stack with an
/// `Arial Unicode MS` entry for exactly that reason -- it is the broadest
/// single face, and it is there to cover the Cyrillic/CJK/Arabic names the
/// primary face lacks, not because anyone wants street names set in Arial.
///
/// This engine does that job differently and better: `MapLabelFallback`
/// picks a face PER SCRIPT (Hiragino for kana, PingFang for Han, Geeza Pro
/// for Arabic), which is what a real system font stack does and what Arial
/// Unicode MS only approximates. So honouring the coverage tail as if it
/// were a design choice would be a double regression -- it would set the
/// Latin labels of every Mapbox Streets style in Arial (the face the
/// engine's default was chosen to get AWAY from) and it would still not
/// improve non-Latin coverage.
///
/// Named explicitly rather than inferred from "the last entry" so that a
/// stack whose tail really is a design alternate keeps it.
const COVERAGE_ONLY_FAMILIES: &[&str] = &["arial unicode ms", "arial unicode"];

/// One `text-font` entry resolved into what a shaper can actually be asked
/// for: a family a font database indexes, plus the weight and slant the
/// entry's name asked for.
///
/// `weight` is the CSS numeric scale (400 regular, 500 medium, 700 bold),
/// which is what `glyphon::Weight` wraps. Stored as a plain `u16` and a
/// plain `bool` rather than cosmic-text's own types so this is a
/// `Hash + Eq` value that a shaping cache can key on without depending on
/// what cosmic-text derives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedFace {
    pub family: std::sync::Arc<str>,
    pub weight: u16,
    pub italic: bool,
}

impl ResolvedFace {
    /// The face for text that has no `text-font` to read: the engine's own
    /// attribution credit, and caller overlays, whose `FeaturePaint` carries
    /// no font property.
    ///
    /// `MEDIUM`, not `NORMAL`, and deliberately: this is the weight the
    /// engine's chrome has always been set in, and it is chrome, not a
    /// style-driven label. A styled label gets its weight from its own
    /// stack; see [`resolve_font_stack`].
    pub fn chrome() -> Self {
        Self { family: DEFAULT_LABEL_FAMILY.into(), weight: 500, italic: false }
    }

    /// The shaping attributes for this face.
    pub fn attrs(&self) -> glyphon::Attrs<'_> {
        glyphon::Attrs::new()
            .family(glyphon::Family::Name(&self.family))
            .weight(glyphon::Weight(self.weight))
            .style(if self.italic { glyphon::Style::Italic } else { glyphon::Style::Normal })
    }
}

/// A `text-font` entry split into the family a font database indexes and the
/// weight/slant its name's suffix asked for.
///
/// `weight` is `None` when the name carries no weight token at all, so a
/// caller can tell `"Noto Sans"` (unstated) from `"Noto Sans Regular"`
/// (stated 400) -- they resolve the same, but only one of them said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceName {
    pub family: String,
    pub weight: Option<u16>,
    pub italic: bool,
}

impl FaceName {
    fn weight_or_regular(&self) -> u16 {
        self.weight.unwrap_or(400)
    }
}

/// What one trailing word of a face name says.
enum StyleWord {
    Weight(u16),
    Italic,
    /// `Condensed`/`Narrow`/`Expanded`. Recognised so it is stripped off the
    /// family rather than left to make the family unmatchable, but not
    /// carried: cosmic-text selects a stretch from `Attrs::stretch`, and
    /// asking for a width this crate cannot verify is installed would pick a
    /// worse face than letting the family's normal width stand.
    Stretch,
}

/// Reads one word of a face name as a weight/slant/width token, or `None`
/// when it is part of the family.
///
/// Case- and separator-insensitive, so `SemiBold`, `Semibold`, `semi-bold`
/// and (via the two-word pass in [`parse_face_name`]) `Semi Bold` are one
/// token. The weights are the CSS numeric scale, which is the scale every
/// font database indexes on.
fn style_word(word: &str) -> Option<StyleWord> {
    let key: String = word.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    Some(match key.as_str() {
        "thin" | "hairline" => StyleWord::Weight(100),
        "extralight" | "ultralight" => StyleWord::Weight(200),
        "light" => StyleWord::Weight(300),
        "regular" | "normal" | "book" | "roman" => StyleWord::Weight(400),
        "medium" => StyleWord::Weight(500),
        "semibold" | "demibold" => StyleWord::Weight(600),
        "bold" => StyleWord::Weight(700),
        "extrabold" | "ultrabold" => StyleWord::Weight(800),
        "black" | "heavy" => StyleWord::Weight(900),
        "italic" | "oblique" => StyleWord::Italic,
        "condensed" | "narrow" | "expanded" | "extended" => StyleWord::Stretch,
        _ => return None,
    })
}

/// Splits a Mapbox font-stack entry into a family and the weight/slant it
/// asked for.
///
/// A style's `text-font` names FACES, not families: `"DIN Pro Medium"`,
/// `"Noto Sans Italic"`, `"Arial Unicode MS Bold"`. That is the glyph-PBF
/// naming convention -- Mapbox's font service keys its `.pbf` ranges by the
/// full face name -- and it is why reading `text-font` naively, as a family,
/// finds nothing installed anywhere: no font database has a family called
/// "DIN Pro Medium". The trailing style words have to come off first.
///
/// Words are consumed from the END while they read as style tokens, two at
/// a time first (`"Extra Bold"`, `"Semi Bold"`) then one, and always leaving
/// at least one word as the family -- so `"Bold"` on its own stays a family
/// name rather than becoming an empty one. That last-word-wins direction is
/// what keeps `"Arial Unicode MS Regular"` resolving to `Arial Unicode MS`
/// and not to `Arial`.
///
/// A family whose own name ends in a style word (`"Arial Black"`,
/// `"PT Sans Narrow"`) is split too, which is the right answer rather than a
/// casualty: `Arial` at weight 900 selects the very face `Arial Black` is,
/// and asking a database for the family plus the weight is how font
/// selection is specified to work.
pub fn parse_face_name(name: &str) -> FaceName {
    let words: Vec<&str> = name.split_whitespace().collect();
    let mut end = words.len();
    let mut weight = None;
    let mut italic = false;
    let apply = |word: StyleWord, weight: &mut Option<u16>, italic: &mut bool| match word {
        // The FIRST weight seen walking backwards wins, so `"Bold Italic"`
        // and a hypothetical `"Bold Medium"` both keep the outermost token.
        StyleWord::Weight(w) => *weight = weight.or(Some(w)),
        StyleWord::Italic => *italic = true,
        StyleWord::Stretch => {}
    };
    while end > 1 {
        if end > 2 {
            if let Some(word) = style_word(&format!("{}{}", words[end - 2], words[end - 1])) {
                apply(word, &mut weight, &mut italic);
                end -= 2;
                continue;
            }
        }
        if let Some(word) = style_word(words[end - 1]) {
            apply(word, &mut weight, &mut italic);
            end -= 1;
            continue;
        }
        break;
    }
    FaceName { family: words[..end].join(" "), weight, italic }
}

/// Picks the face to shape a label in, given the style's `text-font` stack
/// and a predicate saying which families this machine actually has.
///
/// # The rule
///
/// * **Family** comes from the first stack entry whose family is installed,
///   skipping the coverage tails named in [`COVERAGE_ONLY_FAMILIES`], and
///   falling back to [`DEFAULT_LABEL_FAMILY`] when the machine has none of
///   them. An entry is tried both as written (in case a database really does
///   index `"Noto Sans Bold"` as a family, which some do) and as the family
///   [`parse_face_name`] split out of it.
/// * **Weight and slant** always come from the FIRST entry -- the primary
///   face, which is the design statement; everything after it is coverage.
///   They are carried onto whichever family resolves, because weight is an
///   axis a font database SELECTS on: asking for the substitute family at
///   the requested weight is strictly closer than asking for it at 400.
///
/// That split is the whole point. On a machine with none of Mapbox's licensed
/// faces installed -- which is every machine, since they are served as glyph
/// PBFs rather than installed -- honouring the family alone would resolve
/// nothing and the labels would look exactly as they did before this
/// function existed. Honouring the WEIGHT is what makes `DIN Pro Medium`,
/// `Noto Sans Regular`, `Noto Sans Bold` and `Noto Sans Italic` four visibly
/// different labels instead of one, which is most of what a reader actually
/// reads off a map's typographic hierarchy.
pub fn resolve_font_stack(stack: &[String], installed: impl Fn(&str) -> bool) -> ResolvedFace {
    resolve_font_stack_with_default(stack, installed, DEFAULT_LABEL_FAMILY)
}

/// [`resolve_font_stack`], with the family a machine that has none of the
/// stack's faces falls back to spelled out.
///
/// [`DEFAULT_LABEL_FAMILY`] is a macOS system face, and it is the right
/// answer for a build running on macOS: the machine HAS it, so naming it is
/// a real selection. A browser has exactly the faces the page shipped it
/// and `SF Pro Text` is not among them, so falling back to that name there
/// resolves to nothing at all and every label goes through cosmic-text's
/// last-resort database walk instead -- which draws SOMETHING, from
/// whichever face happens to sort first, and is the same non-determinism
/// `MapLabelFallback` exists to remove.
///
/// The weight and slant still come from the stack's FIRST entry, exactly as
/// [`resolve_font_stack`] describes: naming a substitute family does not
/// give up the typographic hierarchy the style asked for.
pub fn resolve_font_stack_with_default(
    stack: &[String],
    installed: impl Fn(&str) -> bool,
    default_family: &str,
) -> ResolvedFace {
    let primary = stack.first().map(|entry| parse_face_name(entry));
    let weight = primary.as_ref().map_or(400, FaceName::weight_or_regular);
    let italic = primary.as_ref().is_some_and(|face| face.italic);
    for entry in stack {
        let parsed = parse_face_name(entry);
        if COVERAGE_ONLY_FAMILIES.contains(&parsed.family.to_ascii_lowercase().as_str()) {
            continue;
        }
        for candidate in [entry.as_str(), parsed.family.as_str()] {
            if installed(candidate) {
                return ResolvedFace { family: candidate.into(), weight, italic };
            }
        }
    }
    ResolvedFace { family: default_family.into(), weight, italic }
}

/// Every family name a font database indexes, ASCII-lowercased, for
/// [`resolve_font_stack`]'s `installed` predicate.
///
/// Built once per font system rather than queried per label: `fontdb` has no
/// family index, so answering "is this family installed?" means walking every
/// face, and a machine has thousands.
///
/// Every name a face declares is taken, not just its first: a face carries
/// one `(name, language)` pair per localisation, and a style is as likely to
/// name the English one as the face's own default.
pub fn installed_families(db: &glyphon::fontdb::Database) -> std::collections::HashSet<String> {
    db.faces().flat_map(|face| face.families.iter().map(|(name, _)| name.to_ascii_lowercase())).collect()
}

/// The shipped label faces, in the order the browser shell loads them
/// (`web/src/app.rs::FONT_URLS`): Noto Sans JP regular and bold (Japan
/// subset), then Noto Sans regular and bold (Latin subset). One list, so a
/// story capture (`story::shipped_label_fonts`) and the browser cannot
/// drift in fallback order.
#[cfg(not(target_arch = "wasm32"))]
// Read by `story::validate`; the iOS shell includes this file by `#[path]`
// without the story player, where it is unused.
#[allow(dead_code)]
pub(crate) const SHIPPED_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/NotoSansJP-Regular.japan-subset.otf"),
    include_bytes!("../assets/fonts/NotoSansJP-Bold.japan-subset.otf"),
    include_bytes!("../assets/fonts/NotoSans-Regular.latin-subset.ttf"),
    include_bytes!("../assets/fonts/NotoSans-Bold.latin-subset.ttf"),
];

/// The story design system's card family (spec §4.1, D5): every Latin,
/// Greek and Cyrillic run. Plain `pub`, no `cfg` -- this is a name, not a
/// font, so it compiles on every target. `story::design` re-exports it
/// (ruling P6) so a story caller never names `font_fallback` directly.
pub const STORY_FAMILY: &str = "Source Sans 3";
/// The story design system's Arabic-script family (spec §4.1, D5): 400
/// and 700 only, 600 maps to 700. Plain `pub`, no `cfg`, for the same
/// reason as [`STORY_FAMILY`].
pub const STORY_ARABIC_FAMILY: &str = "IBM Plex Sans Arabic";

/// The story card's five faces: Source Sans 3 at 400/600/700 and IBM Plex
/// Sans Arabic at 400/700 (spec §4.1, D5), OFL, subset -- see
/// `assets/fonts/README.md` for provenance and the exact `pyftsubset`
/// invocations. A separate list from [`SHIPPED_FONTS`]: the `WEB_FONTS`
/// test above pins that list byte for byte to the browser's `FONT_URLS`,
/// and the browser has no story player. Not on iOS either (no iOS story
/// player, and +500 KB on the native side already -- see below). Native
/// only (ruling P6).
///
/// Combined, these five files are 515,648 bytes (≈503.6 KB), not the
/// spec's and the plan's original "~350 KB" estimate -- that number was
/// never measured against the real upstream releases. The measured total
/// is native-only: it adds ≈0.5 MB to a roughly 70 MB desktop/capture
/// binary and reaches neither the browser nor iOS, which is why the
/// budget below is 520 KB rather than 400 KB (coordinator ruling,
/// 2026-10-04): raise the budget and keep every weight and every script
/// this spec promises, rather than trim Source Sans 3's Latin/Greek/
/// Cyrillic coverage or Plex Arabic's joining-form glyphs to fit a
/// smaller number.
#[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
pub(crate) const STORY_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/SourceSans3-Regular.latin-subset.ttf"),
    include_bytes!("../assets/fonts/SourceSans3-Semibold.latin-subset.ttf"),
    include_bytes!("../assets/fonts/SourceSans3-Bold.latin-subset.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSansArabic-Regular.arabic-subset.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSansArabic-Bold.arabic-subset.ttf"),
];

// ---------------------------------------------------------------------
// TextStyle (story design system, spec §4.1, §8): engine-side because
// `symbol_pass::LabelKey` carries one (R12) and `symbol_pass.rs`
// is compiled into the iOS and browser shells, which must never depend
// on a story module (ruling P6). `story::design` re-exports this.
// ---------------------------------------------------------------------

/// A label or card text run's shaping: a weight, a tracking (letter
/// spacing) and a bitset of OpenType features. Distinct from a
/// [`ResolvedFace`] -- that is which face to shape with; this is how to
/// shape it once a face is chosen.
///
/// `Default` (every field zero) is the plain map-label style: weight 0
/// reads as "the face's own", zero tracking, no features turned on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct TextStyle {
    pub weight: u16,
    pub letter_spacing_milli_em: i16,
    pub features: u8,
}

impl TextStyle {
    /// A style that only names a weight: no tracking, no features.
    #[must_use]
    pub const fn of_weight(weight: u16) -> Self {
        Self { weight, letter_spacing_milli_em: 0, features: 0 }
    }

    /// This style with `features` (a [`TNUM`] / [`LNUM`] bitset) turned on
    /// as well.
    #[must_use]
    pub const fn with_features(self, features: u8) -> Self {
        Self { features: self.features | features, ..self }
    }

    /// This style as it shapes `text` (spec §4.1, §8): tracking forced to
    /// 0 on a run with any Arabic, Hebrew, other joining-script, Indic or
    /// CJK character ([`takes_no_tracking`]) -- tracking breaks joining
    /// and conjuncts, and CJK is set solid. The shaping key
    /// carries the result, so a tracked token on an Arabic headline and
    /// the same headline untracked are one cache entry.
    #[must_use]
    pub fn for_text(self, text: &str) -> Self {
        if self.letter_spacing_milli_em == 0 || !text.chars().any(takes_no_tracking) {
            return self;
        }
        Self { letter_spacing_milli_em: 0, ..self }
    }

    /// `face`'s shaping attributes with this style applied: the weight
    /// when one is named, the tracking (in em) and the features. The
    /// default style changes nothing, so it shapes exactly as `face`
    /// alone always has.
    pub fn apply<'a>(self, attrs: glyphon::Attrs<'a>) -> glyphon::Attrs<'a> {
        let mut attrs = attrs;
        if self.weight != 0 {
            attrs = attrs.weight(glyphon::Weight(self.weight));
        }
        if self.letter_spacing_milli_em != 0 {
            attrs = attrs.letter_spacing(f32::from(self.letter_spacing_milli_em) / 1000.0);
        }
        if self.features != 0 {
            let mut features = glyphon::cosmic_text::FontFeatures::new();
            if self.features & TNUM != 0 {
                features.enable(glyphon::cosmic_text::FeatureTag::new(b"tnum"));
            }
            if self.features & LNUM != 0 {
                features.enable(glyphon::cosmic_text::FeatureTag::new(b"lnum"));
            }
            attrs = attrs.font_features(features);
        }
        attrs
    }

    /// The tracking, in em.
    #[must_use]
    pub fn letter_spacing_em(self) -> f32 {
        f32::from(self.letter_spacing_milli_em) / 1000.0
    }
}

/// Whether `c` is in a script tracking must not touch (spec §4.1):
/// Arabic and Hebrew; the other joining scripts (Syriac, N'Ko, Mongolian,
/// Thaana), where spacing breaks the connected letterforms; the Indic
/// scripts, whose headstrokes and conjuncts read as broken when spaced;
/// and CJK (Han, kana, Hangul, Bopomofo), which is set solid.
fn takes_no_tracking(c: char) -> bool {
    use unicode_script::UnicodeScript as _;
    matches!(
        c.script(),
        Script::Arabic
            | Script::Hebrew
            | Script::Syriac
            | Script::Nko
            | Script::Mongolian
            | Script::Thaana
            | Script::Devanagari
            | Script::Bengali
            | Script::Gurmukhi
            | Script::Gujarati
            | Script::Oriya
            | Script::Tamil
            | Script::Telugu
            | Script::Kannada
            | Script::Malayalam
            | Script::Sinhala
            | Script::Han
            | Script::Hiragana
            | Script::Katakana
            | Script::Hangul
            | Script::Bopomofo
    )
}

/// [`TextStyle::features`]: tabular (fixed-width) numerals, for a stat,
/// a counter or a legend value that must not jitter as its digits
/// change (spec §4.1).
pub const TNUM: u8 = 1;
/// [`TextStyle::features`]: lining numerals (as opposed to old-style),
/// for a numeral drawn beside capitals or in a headline.
pub const LNUM: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// **The default label face is installed, so the chrome's face does not
    /// depend on directory order.** Without it every chrome label falls to
    /// the database walk, whose first face is whatever the filesystem lists
    /// first (see [`DEFAULT_LABEL_FAMILY`]). This fails on a Linux machine
    /// without DejaVu Sans, CI included, which is the point: the CI image
    /// installs `fonts-dejavu-core`, and losing it brings the flake back.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_default_label_family_is_installed() {
        let mut db = glyphon::fontdb::Database::new();
        db.load_system_fonts();
        assert!(
            installed_families(&db).contains(&DEFAULT_LABEL_FAMILY.to_ascii_lowercase()),
            "{DEFAULT_LABEL_FAMILY} is not installed (Debian/Ubuntu: fonts-dejavu-core)"
        );
    }

    /// The one regression this module must not cause. Latin, Greek and
    /// Cyrillic are covered by the requested family, so the fallback table
    /// must have nothing to say about them -- an entry here would let a
    /// Singapore street name be drawn by some other face.
    #[test]
    fn the_scripts_the_requested_family_covers_ask_for_no_fallback_at_all() {
        for script in [Script::Latin, Script::Greek, Script::Cyrillic] {
            for locale in ["en-US", "ja-JP", "zh-CN", "el-GR", "ru-RU"] {
                assert!(
                    script_fallback_families(script, locale).is_empty(),
                    "{script:?} under {locale:?} must resolve through the requested family, not a fallback"
                );
            }
        }
    }

    /// macOS parity, stated as a test rather than as a promise: on a
    /// machine whose locale is not a CJK one -- the machine this was
    /// developed on reports `en-SG` -- Han must still resolve to
    /// `PingFang SC` first, which is exactly what cosmic-text's macOS table
    /// resolved to before this module replaced it.
    #[test]
    fn a_non_cjk_locale_still_lands_on_the_same_han_face_cosmic_text_chose() {
        for locale in ["en-SG", "en-US", "de-DE", "ru-RU"] {
            assert_eq!(script_fallback_families(Script::Han, locale).first(), Some(&"PingFang SC"));
        }
    }

    /// The bug in cosmic-text's own table: it compares the locale to the
    /// literal `"ja"`, so a real Japanese system reporting `ja-JP` misses
    /// every arm and draws Japanese place names in a Simplified Chinese
    /// face. Region and script subtags must not cost a locale its language.
    #[test]
    fn a_locale_with_a_region_or_script_subtag_still_picks_its_own_han_face() {
        let first = |locale| script_fallback_families(Script::Han, locale).first().copied();
        assert_eq!(first("ja"), Some("Hiragino Sans"));
        assert_eq!(first("ja-JP"), Some("Hiragino Sans"));
        assert_eq!(first("ko-KR"), Some("Apple SD Gothic Neo"));
        assert_eq!(first("zh-TW"), Some("PingFang TC"));
        assert_eq!(first("zh-Hant-TW"), Some("PingFang TC"));
        assert_eq!(first("zh-HK"), Some("PingFang HK"));
        assert_eq!(first("zh-CN"), Some("PingFang SC"));
        assert_eq!(first("zh-Hans-CN"), Some("PingFang SC"));
        // Case is not meaningful in BCP 47.
        assert_eq!(first("ZH-HANT-TW"), Some("PingFang TC"));
        // `java` is not `ja`: prefix matching must respect subtag
        // boundaries, or a language tag could capture an unrelated one.
        assert_eq!(first("javanese"), Some("PingFang SC"));
    }

    /// Kana is only ever Japanese and Hangul only ever Korean, so neither
    /// may be routed through Han unification -- a Japanese street name on a
    /// Chinese-locale machine must still get Japanese kana.
    #[test]
    fn kana_and_hangul_pin_their_language_regardless_of_locale() {
        for locale in ["zh-CN", "en-US", "ko-KR"] {
            assert_eq!(script_fallback_families(Script::Hiragana, locale).first(), Some(&"Hiragino Sans"));
            assert_eq!(script_fallback_families(Script::Katakana, locale).first(), Some(&"Hiragino Sans"));
        }
        for locale in ["zh-CN", "en-US", "ja-JP"] {
            assert_eq!(script_fallback_families(Script::Hangul, locale).first(), Some(&"Apple SD Gothic Neo"));
        }
    }

    /// The point of the module: a list that names only Apple faces would
    /// leave iOS working and Windows, Linux and wasm drawing nothing. Every
    /// script this table claims to cover must name a portable face too --
    /// `Noto` or a Microsoft one -- not just the macOS entry cosmic-text
    /// already had.
    #[test]
    fn every_covered_script_names_a_face_from_more_than_one_platform() {
        let covered = [
            Script::Han,
            Script::Hiragana,
            Script::Katakana,
            Script::Hangul,
            Script::Arabic,
            Script::Hebrew,
            Script::Devanagari,
            Script::Bengali,
            Script::Tamil,
            Script::Telugu,
            Script::Thai,
            Script::Khmer,
            Script::Myanmar,
            Script::Armenian,
            Script::Georgian,
            Script::Ethiopic,
        ];
        for script in covered {
            let families = script_fallback_families(script, "en-US");
            assert!(!families.is_empty(), "{script:?} is claimed as covered but names no family");
            let portable = families
                .iter()
                .any(|f| f.starts_with("Noto") || f.starts_with("Microsoft") || f.starts_with("Segoe") || f.starts_with("Source Han"));
            assert!(portable, "{script:?} names only Apple faces: {families:?} -- iOS aside, nothing else would resolve");
        }
    }

    /// The floor. A script with no entry, and an unassigned codepoint's
    /// `Script::Unknown`, must answer with an empty list rather than a
    /// guess or a panic: an empty list hands the codepoint to the common
    /// list and then to cosmic-text's exhaustive walk, and a glyph nothing
    /// covers is simply not drawn.
    #[test]
    fn an_uncovered_script_degrades_to_an_empty_list_rather_than_a_guess() {
        assert!(script_fallback_families(Script::Unknown, "en-US").is_empty());
        assert!(script_fallback_families(Script::Common, "ja-JP").is_empty());
        assert!(script_fallback_families(Script::Linear_A, "en-US").is_empty());
    }

    /// `.LastResort` draws a box for every codepoint in Unicode. Reaching
    /// it turns a missing glyph into a row of tofu, which is worse than a
    /// gap, so it has to stay named as forbidden.
    #[test]
    fn the_tofu_font_stays_forbidden() {
        assert!(FORBIDDEN_FALLBACK.contains(&".LastResort"));
    }

    /// The common list is consulted for anything no script list covered, so
    /// it has to open with the macOS entries cosmic-text used -- in
    /// cosmic-text's order -- for macOS resolution to be unchanged.
    /// A style's `text-font` names FACES, not families. Reading one as a
    /// family finds nothing installed anywhere -- no database has a family
    /// called "DIN Pro Medium" -- so the trailing style words have to come
    /// off before anything can be looked up.
    #[test]
    fn a_face_name_splits_into_the_family_a_database_indexes_and_the_weight_it_asked_for() {
        let cases = [
            ("Noto Sans Regular", "Noto Sans", Some(400), false),
            ("DIN Pro Medium", "DIN Pro", Some(500), false),
            ("DIN Pro Italic", "DIN Pro", None, true),
            ("DIN Pro Bold", "DIN Pro", Some(700), false),
            ("Inter Regular", "Inter", Some(400), false),
            // A slant with no weight beside it leaves the weight UNSTATED --
            // `resolve_font_stack` reads that as Regular, but the split must
            // not invent a token the name did not carry.
            ("Inter Italic", "Inter", None, true),
            ("Montserrat SemiBold", "Montserrat", Some(600), false),
            ("Montserrat Semi Bold", "Montserrat", Some(600), false),
            ("Open Sans Extra Bold", "Open Sans", Some(800), false),
            ("Noto Sans Bold Italic", "Noto Sans", Some(700), true),
            ("Noto Sans Light Oblique", "Noto Sans", Some(300), true),
            // Three family words, and the LAST word is the style one -- the
            // split must not walk in from the front and answer "Arial".
            ("Arial Unicode MS Regular", "Arial Unicode MS", Some(400), false),
            ("Arial Unicode MS Bold", "Arial Unicode MS", Some(700), false),
            // No style word at all: a family, stated as one.
            ("Helvetica Neue", "Helvetica Neue", None, false),
            // A family whose own name ends in a style word. Splitting it is
            // the right answer: `Arial` at 900 selects the face `Arial
            // Black` IS.
            ("Arial Black", "Arial", Some(900), false),
        ];
        for (name, family, weight, italic) in cases {
            assert_eq!(
                parse_face_name(name),
                FaceName { family: family.into(), weight, italic },
                "{name:?} did not split the way a font database indexes it",
            );
        }
    }

    /// The floor on the split: it consumes from the end, so a name that is
    /// ALL style words would leave no family at all. One word always stays.
    #[test]
    fn a_face_name_that_is_nothing_but_style_words_still_keeps_a_family() {
        assert_eq!(parse_face_name("Bold").family, "Bold");
        assert_eq!(parse_face_name("Italic").family, "Italic");
        // Not reachable from a style document, but neither is a panic
        // acceptable for a string a style author controls.
        assert_eq!(parse_face_name("").family, "");
    }

    /// The point of `resolve_font_stack`: on a machine with none of Mapbox's
    /// licensed faces installed -- which is EVERY machine, since they are
    /// served as glyph PBFs rather than installed -- the family cannot
    /// resolve, but the WEIGHT still can, and the weight is most of what a
    /// reader reads off a map's typographic hierarchy.
    #[test]
    fn an_uninstalled_family_still_carries_its_weight_onto_the_default_face() {
        let nothing_installed = |_: &str| false;
        for (entry, weight, italic) in [
            ("DIN Pro Medium", 500, false),
            ("Noto Sans Regular", 400, false),
            ("Noto Sans Bold", 700, false),
            ("Noto Sans Italic", 400, true),
            ("Montserrat SemiBold", 600, false),
        ] {
            let face = resolve_font_stack(&[entry.to_string()], nothing_installed);
            assert_eq!(face.family.as_ref(), DEFAULT_LABEL_FAMILY, "{entry}");
            assert_eq!(face.weight, weight, "{entry}");
            assert_eq!(face.italic, italic, "{entry}");
        }
    }

    /// A family the machine DOES have wins, and takes the primary entry's
    /// weight with it -- weight is an axis a font database selects on, so
    /// asking for the substitute at the requested weight is strictly closer
    /// than asking for it at 400.
    #[test]
    fn the_first_installed_family_in_the_stack_wins_and_keeps_the_primary_weight() {
        let only_helvetica = |name: &str| name == "Helvetica Neue";
        let face = resolve_font_stack(
            &["DIN Pro Medium".into(), "Helvetica Neue Regular".into()],
            only_helvetica,
        );
        assert_eq!(face.family.as_ref(), "Helvetica Neue");
        assert_eq!(face.weight, 500, "the weight is the PRIMARY entry's, not the substitute's");
    }

    /// Mapbox stacks are per-codepoint coverage chains and every one of them
    /// ends in `Arial Unicode MS` -- which macOS HAS. Honouring it as a
    /// design choice would set every Mapbox Streets label in Arial, the face
    /// `DEFAULT_LABEL_FAMILY` was picked to get away from, while not
    /// improving non-Latin coverage at all (`script_fallback_families`
    /// already picks a better face per script).
    #[test]
    fn the_coverage_tail_of_a_mapbox_stack_never_becomes_the_labels_typeface() {
        let arial_unicode_installed = |name: &str| name.eq_ignore_ascii_case("Arial Unicode MS");
        for stack in [
            vec!["DIN Pro Medium".to_string(), "Arial Unicode MS Regular".to_string()],
            vec!["Inter Regular".to_string(), "Arial Unicode MS Regular".to_string()],
            vec!["Montserrat SemiBold".to_string(), "Arial Unicode MS Bold".to_string()],
        ] {
            let face = resolve_font_stack(&stack, arial_unicode_installed);
            assert_eq!(
                face.family.as_ref(),
                DEFAULT_LABEL_FAMILY,
                "{stack:?} resolved to its coverage tail instead of the engine default",
            );
        }
    }

    /// The Standard styles write their primary entry as an EXPRESSION
    /// (`["concat", ["config", "font"], " Medium"]`). A compiler that cannot
    /// fold it hands this function a stack with the expression dropped, and
    /// the remaining entry is the coverage tail -- which must still not
    /// become the typeface, and must not drag the label to its own weight.
    #[test]
    fn a_stack_left_with_only_a_coverage_tail_falls_back_rather_than_setting_arial() {
        let arial_unicode_installed = |name: &str| name.eq_ignore_ascii_case("Arial Unicode MS");
        let face = resolve_font_stack(&["Arial Unicode MS Bold".to_string()], arial_unicode_installed);
        assert_eq!(face.family.as_ref(), DEFAULT_LABEL_FAMILY);
        // The tail IS the primary here, so its weight is all there is to go
        // on -- taking it is right; taking its FAMILY is not.
        assert_eq!(face.weight, 700);
    }

    /// An empty stack is reachable (a `text-font` whose every entry the
    /// compiler refused) and must resolve, not panic.
    #[test]
    fn an_empty_stack_resolves_to_the_default_face_at_regular() {
        let face = resolve_font_stack(&[], |_| false);
        assert_eq!(face.family.as_ref(), DEFAULT_LABEL_FAMILY);
        assert_eq!(face.weight, 400);
        assert!(!face.italic);
    }

    /// The engine's own chrome has no `text-font` to read and has always been
    /// set in Medium. Pinned so that making labels style-driven cannot
    /// silently restyle the attribution credit too.
    #[test]
    fn the_chrome_face_is_the_medium_default_the_credit_has_always_used() {
        let face = ResolvedFace::chrome();
        assert_eq!(face.family.as_ref(), DEFAULT_LABEL_FAMILY);
        assert_eq!(face.weight, 500);
        assert!(!face.italic);
    }

    /// A face carrying a weight and a slant has to hand cosmic-text BOTH --
    /// a stack that says `Italic` and shapes upright is the defect this
    /// whole path exists to fix.
    #[test]
    fn a_resolved_face_hands_cosmic_text_its_family_weight_and_slant() {
        let face = ResolvedFace { family: "DIN Pro".into(), weight: 500, italic: true };
        let attrs = face.attrs();
        assert!(matches!(attrs.family, glyphon::Family::Name(name) if name == "DIN Pro"));
        assert_eq!(attrs.weight, glyphon::Weight(500));
        assert_eq!(attrs.style, glyphon::Style::Italic);
        assert_eq!(ResolvedFace { family: "DIN Pro".into(), weight: 400, italic: false }.attrs().style, glyphon::Style::Normal);
    }

    /// The style spec's own default stack, resolved on a machine that has
    /// neither of its faces as a DESIGN face. Every symbol layer with no
    /// `text-font` goes through this, so it has to land on the engine default
    /// at Regular -- not on Arial, and not at the Medium the hardcoded
    /// constant used to impose on every label in every style.
    #[test]
    fn the_spec_default_stack_lands_on_the_default_face_at_regular() {
        let stack: Vec<String> = DEFAULT_FONT_STACK.iter().map(|s| s.to_string()).collect();
        let face = resolve_font_stack(&stack, |name| name.eq_ignore_ascii_case("Arial Unicode MS"));
        assert_eq!(face.family.as_ref(), DEFAULT_LABEL_FAMILY);
        assert_eq!(face.weight, 400);
    }

    #[test]
    fn the_common_list_opens_with_the_macos_entries_it_replaces() {
        assert_eq!(&COMMON_FALLBACK[..5], &[".SF NS", "Menlo", "Apple Color Emoji", "Geneva", "Arial Unicode MS"]);
    }

    // -----------------------------------------------------------------------
    // The shipped font sets: what the browser and iOS shells shape with
    // -----------------------------------------------------------------------

    /// The four faces `web/src/app.rs::FONT_URLS` fetches, byte for byte.
    const WEB_FONTS: &[&[u8]] = super::SHIPPED_FONTS;

    /// The one face `ios/src/app.rs::EMBEDDED_FONT` embeds.
    const IOS_FONTS: &[&[u8]] = &[include_bytes!("../assets/fonts/NotoSans-Regular.latin-subset.ttf")];

    /// Vietnamese place names as the Mapbox Streets tiles over Ho Chi Minh
    /// City carry them, one per failure the report showed: Latin
    /// Extended-B (ơ U+01A1, ư U+01B0), Latin Extended Additional
    /// (U+1EA0-1EF9: ủ ầ ộ ồ ợ ệ), and Latin Extended-A beside them (ă Đ),
    /// which always drew. The last is the same name DECOMPOSED -- base
    /// letters plus combining marks (U+0300-036F) -- because a tile is
    /// under no obligation to ship NFC.
    const VIETNAMESE: &[&str] = &[
        "Thủ Dầu Một",
        "Bình Dương",
        "Đồng Nai",
        "Tăng Nhơn Phú",
        "Hòa Lợi",
        "Thạnh Mỹ Lợi",
        "Thu\u{309} D\u{302}\u{300}u Mo\u{302}\u{323}t",
    ];

    /// Shapes `text` the way `symbol_pass` does -- the Mapbox Streets
    /// stack resolved against what `db` has installed, falling back to
    /// `default_family` -- and returns every char no face drew: a char
    /// whose byte range no glyph with a real (non-`.notdef`) id covers.
    fn chars_without_a_glyph(files: &[&[u8]], default_family: &str, weight_entry: &str, text: &str) -> Vec<char> {
        let mut db = glyphon::fontdb::Database::new();
        for bytes in files {
            db.load_font_data(bytes.to_vec());
        }
        let installed = installed_families(&db);
        let mut font_system = new_font_system_from_db(String::from("en-US"), db);
        let stack = vec![weight_entry.to_string(), "Arial Unicode MS Regular".to_string()];
        let face = resolve_font_stack_with_default(&stack, |name| installed.contains(&name.to_ascii_lowercase()), default_family);
        let mut buffer = glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(16.0, 20.0));
        buffer.set_text(text, &face.attrs(), glyphon::Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut font_system, false);
        let glyphs: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs.iter().cloned()).collect();
        text.char_indices()
            .filter(|(_, c)| !c.is_whitespace())
            .filter(|(at, _)| !glyphs.iter().any(|g| g.glyph_id != 0 && g.start <= *at && *at < g.end && g.w > 0.0))
            .map(|(_, c)| c)
            .collect()
    }

    /// The report: over Ho Chi Minh City the browser drew "Thủ Dầu Một" as
    /// "Th D u M t". Every char of every Vietnamese name must come out of
    /// SOME face the page shipped, at both weights the page shipped.
    #[test]
    fn every_char_of_a_vietnamese_name_has_a_glyph_in_the_web_font_set() {
        for entry in ["DIN Pro Regular", "DIN Pro Bold"] {
            for name in VIETNAMESE {
                let missing = chars_without_a_glyph(WEB_FONTS, "Noto Sans JP", entry, name);
                assert!(missing.is_empty(), "web, {entry}: {name:?} has no glyph for {missing:?}");
            }
        }
    }

    /// The same, for the one face the iOS shell embeds and shapes with.
    #[test]
    fn every_char_of_a_vietnamese_name_has_a_glyph_in_the_ios_font_set() {
        for name in VIETNAMESE {
            let missing = chars_without_a_glyph(IOS_FONTS, "Noto Sans", "DIN Pro Medium", name);
            assert!(missing.is_empty(), "iOS: {name:?} has no glyph for {missing:?}");
        }
    }

    // -----------------------------------------------------------------------
    // The story card's faces (spec §4.1, D5): native only (ruling P6), so
    // these two tests are gated with the same `cfg` as `STORY_FONTS`.
    // -----------------------------------------------------------------------

    /// Every weight the design tokens (§4.1) need is present, and the
    /// bundle stays within budget. 520 KB, not the spec's and the plan's
    /// original "~350 KB": these faces are native-only, so the browser and
    /// iOS pay nothing for them, and ≈0.5 MB is a rounding error on a
    /// roughly 70 MB desktop binary. The real total measured here, with
    /// `--no-hinting` (assets/fonts/README.md), is 515,648 bytes; the 520 KB
    /// cap leaves a small margin without inviting a weight no face needs.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn the_story_faces_are_bundled_at_their_weights_and_stay_small() {
        let mut db = glyphon::fontdb::Database::new();
        for bytes in STORY_FONTS {
            db.load_font_data(bytes.to_vec());
        }
        let has = |family: &str, weight: u16| {
            db.faces().any(|f| f.families.iter().any(|(n, _)| n == family) && f.weight.0 == weight)
        };
        for w in [400, 600, 700] {
            assert!(has(STORY_FAMILY, w), "Source Sans 3 {w}");
        }
        for w in [400, 700] {
            assert!(has(STORY_ARABIC_FAMILY, w), "Plex Arabic {w}");
        }
        let story_bytes: usize = STORY_FONTS.iter().map(|b| b.len()).sum();
        assert!(story_bytes <= 520 * 1024, "{story_bytes} bytes");
        assert_eq!(SHIPPED_FONTS.len(), 4, "the browser's label faces are a separate list and do not grow");
    }

    /// The native `FontSystem` -- built by [`new_font_system`], which both
    /// the capture session and the live map use -- carries both story
    /// families alongside the system fonts.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn the_native_font_system_carries_the_story_families() {
        let fs = new_font_system();
        let families = installed_families(fs.db());
        assert!(
            families.contains(&STORY_FAMILY.to_ascii_lowercase()) && families.contains(&STORY_ARABIC_FAMILY.to_ascii_lowercase()),
            "{families:?}"
        );
    }
}

/// Reproduces `ios/src/app.rs`'s (and `web/src/app.rs`'s) font world exactly:
/// a `fontdb::Database` holding NOTHING but the one embedded Latin-subset
/// face, `db.load_system_fonts()` never called -- see `EMBEDDED_FONT`'s doc
/// comment on `ios/src/app.rs` for why that call is the wrong tool on iOS
/// (no `target_os` arm; `wasm32` is the same story). Real bytes, not a
/// stand-in: `include_bytes!` of the exact file both shells ship.
#[cfg(test)]
mod ios_style_font_stacks_over_the_embedded_face_only {
    use super::*;

    /// The one face iOS/web actually have, loaded the same way `Labels::new`
    /// loads it.
    fn embedded_only_db() -> glyphon::fontdb::Database {
        let bytes = include_bytes!("../assets/fonts/NotoSans-Regular.latin-subset.ttf").to_vec();
        let mut db = glyphon::fontdb::Database::new();
        db.load_font_data(bytes);
        db
    }

    /// Shapes `text` with `face` against `db` and returns the `fontdb::ID`
    /// every produced glyph actually came from -- empty if shaping produced
    /// no runs at all, which is the "rendered nothing" failure this module
    /// exists to prevent.
    fn shaped_glyph_font_ids(db: glyphon::fontdb::Database, face: &ResolvedFace, text: &str) -> Vec<glyphon::fontdb::ID> {
        let mut font_system =
            glyphon::FontSystem::new_with_locale_and_db_and_fallback("en-US".to_string(), db, MapLabelFallback);
        let mut buffer = glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(14.0, 18.0));
        buffer.set_text(text, &face.attrs(), glyphon::Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut font_system, false);
        buffer.layout_runs().flat_map(|run| run.glyphs.iter().map(|g| g.font_id).collect::<Vec<_>>()).collect()
    }

    /// The headline defect this module exists to catch, reproduced exactly:
    /// `mapbox-streets-style.json`'s `road-label`/`road-intersection` family
    /// of layers asks for `["DIN Pro Medium", "Arial Unicode MS Regular"]`
    /// (see `examples/mapbox-streets-style.json`). Neither face is installed
    /// on a build that only embeds one Latin subset -- and `Arial Unicode
    /// MS` is deliberately never a candidate at all, being coverage-only
    /// (`COVERAGE_ONLY_FAMILIES`) -- so this must resolve to the embedded
    /// face's own family (`FONT_FAMILY` in `ios/src/app.rs`/`web/src/app.rs`,
    /// "Noto Sans") and that resolution must actually SHAPE: every glyph of
    /// "Main Street" has to come back attached to a real font id, not an
    /// empty run.
    #[test]
    fn a_mapbox_streets_font_stack_resolves_to_the_embedded_face_and_actually_shapes() {
        let db = embedded_only_db();
        let installed = installed_families(&db);
        let stack = vec!["DIN Pro Medium".to_string(), "Arial Unicode MS Regular".to_string()];
        let face = resolve_font_stack_with_default(&stack, |n| installed.contains(&n.to_ascii_lowercase()), "Noto Sans");
        assert_eq!(face.family.as_ref(), "Noto Sans", "must degrade to the embedded face, not DIN Pro or Arial Unicode MS");
        assert_eq!(face.weight, 500, "the PRIMARY entry's weight (Medium) must survive the family substitution");

        let font_ids = shaped_glyph_font_ids(db, &face, "Main Street");
        assert_eq!(font_ids.len(), "Main Street".chars().count(), "every character must shape into a glyph");
        assert!(font_ids.iter().all(|id| *id == font_ids[0]), "every glyph must come from the same (only) embedded face");
    }

    /// The negative control the task asks for: a family the machine already
    /// has must still resolve to itself, not be dragged onto the default by
    /// mistake -- exactly what the two OMT styles' `"Noto Sans Regular"` /
    /// `"Noto Sans Bold"` / `"Noto Sans Italic"` requests rely on.
    #[test]
    fn a_font_stack_already_naming_the_embedded_face_resolves_to_itself_and_shapes() {
        let db = embedded_only_db();
        let installed = installed_families(&db);
        let stack = vec!["Noto Sans Regular".to_string()];
        let face = resolve_font_stack_with_default(&stack, |n| installed.contains(&n.to_ascii_lowercase()), "Noto Sans");
        assert_eq!(face.family.as_ref(), "Noto Sans");
        assert_eq!(face.weight, 400);

        let font_ids = shaped_glyph_font_ids(db, &face, "Main Street");
        assert_eq!(font_ids.len(), "Main Street".chars().count());
        assert!(font_ids.iter().all(|id| *id == font_ids[0]));
    }

    /// Every REAL compiled symbol layer of the vendored Mapbox Streets style
    /// -- not a hand-typed stand-in -- must resolve to a face this build can
    /// actually shape with. Guards against the font-facing half of this
    /// defect regressing even if a future style edit changes which layers
    /// compile or what their `text-font` stacks say.
    #[test]
    fn every_compiled_mapbox_streets_symbol_layer_resolves_to_the_embedded_face() {
        // The monorepo vendors the fixture in `examples/`; the open library
        // cut drops it, and finds it only in `$MAPBOX_FIXTURES`.
        let vendored = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/mapbox-streets-style.json");
        let Some(bytes) = std::fs::read(vendored).ok().or_else(|| crate::mapbox_fixtures::read("mapbox-streets-style.json")) else {
            return;
        };
        let style = crate::style_spec::parse_and_compile(&bytes).expect("vendored fixture must compile");
        let db = embedded_only_db();
        let installed = installed_families(&db);
        let mut checked = 0usize;
        for layer in &style.layers {
            let crate::style::StyleLayer::Symbol { id, text_font, .. } = layer else { continue };
            let face = resolve_font_stack_with_default(text_font, |n| installed.contains(&n.to_ascii_lowercase()), "Noto Sans");
            assert_eq!(face.family.as_ref(), "Noto Sans", "layer {id:?}'s text-font {text_font:?} did not resolve to the embedded face");
            checked += 1;
        }
        assert!(checked > 0, "the vendored fixture must still contain compiled symbol layers to check");
    }
}
