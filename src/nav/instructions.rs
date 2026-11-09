//! Turn text from OSRM maneuvers: Project OSRM's `osrm-text-instructions`
//! (v0.13.4, BSD-2-Clause, Copyright (c) 2016, Project OSRM contributors),
//! ported to Rust for English. `osrm_text_instructions/en.json` is that
//! project's `languages/translations/en.json`, verbatim, and its licence is
//! beside it in `osrm_text_instructions/LICENSE.txt`.
//!
//! `compile` is `index.js`'s `compile` line for line -- the branch order,
//! the token names, JavaScript's truthiness (an empty string is "absent"),
//! the capitalisation rule and the double-space collapse -- because the test
//! below holds it to every one of that project's 630 English fixtures, and
//! a paraphrase would not pass them.
//!
//! [`attach`] then fills each Ferrostar step's `visual_instructions` with the
//! UPCOMING maneuver's sentence, which is what a banner shows (Mapbox's
//! `bannerInstructions` convention, and what Ferrostar's
//! `get_active_visual_instruction` expects to find).

use crate::nav::route_source::{NavRoute, OsrmStep};
use ferrostar::models::{ManeuverModifier, ManeuverType, VisualInstruction, VisualInstructionContent};
use serde_json::Value;
use std::sync::OnceLock;

const EN_JSON: &str = include_str!("osrm_text_instructions/en.json");
const NOT_MANEUVER_TYPES: [&str; 3] = ["constants", "modes", "phrase"];

fn v5() -> &'static Value {
    static TABLE: OnceLock<Value> = OnceLock::new();
    &TABLE.get_or_init(|| serde_json::from_str(EN_JSON).expect("the vendored phrase table is JSON"))["v5"]
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CompileOptions<'a> {
    pub leg_index: Option<usize>,
    pub leg_count: Option<usize>,
    pub classes: &'a [String],
    pub waypoint_name: Option<&'a str>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

pub fn compile(step: &OsrmStep, options: &CompileOptions) -> String {
    let v5 = v5();
    let mut kind = step.maneuver.kind.as_str();
    if kind.is_empty() {
        return String::new();
    }
    let modifier = non_empty(step.maneuver.modifier.as_deref());
    if v5.get(kind).is_none() || NOT_MANEUVER_TYPES.contains(&kind) {
        kind = "turn";
    }
    let mut object: &Value = match step.mode.as_deref().and_then(|mode| v5["modes"].get(mode)) {
        Some(mode_object) => mode_object,
        None => {
            let omit_side = kind == "off ramp"
                && matches!((modifier, step.driving_side.as_deref()), (Some(m), Some(side)) if m.contains(side));
            match modifier.and_then(|m| v5[kind].get(m)) {
                Some(by_modifier) if !omit_side => by_modifier,
                _ => &v5[kind]["default"],
            }
        }
    };
    let mut lane_instruction: Option<String> = None;
    match kind {
        "use lane" => {
            lane_instruction =
                v5["constants"]["lanes"].get(lane_config(step)).and_then(Value::as_str).map(str::to_string);
            if lane_instruction.is_none() {
                object = &v5["use lane"]["no_lanes"];
            }
        }
        "rotary" | "roundabout" => {
            let named = non_empty(step.rotary_name.as_deref()).is_some();
            let exit = step.maneuver.exit.is_some_and(|e| e != 0);
            let has = |key: &str| object.get(key).is_some_and(Value::is_object);
            object = if named && exit && has("name_exit") {
                &object["name_exit"]
            } else if named && has("name") {
                &object["name"]
            } else if exit && has("exit") {
                &object["exit"]
            } else {
                &object["default"]
            };
        }
        _ => {}
    }

    let way_name = way_name(step, options.classes);
    let destinations = non_empty(step.destinations.as_deref());
    let exits = non_empty(step.exits.as_deref());
    let template = |key: &str| object.get(key).and_then(Value::as_str);
    let instruction = match (destinations, exits) {
        (Some(_), Some(_)) if template("exit_destination").is_some() => template("exit_destination"),
        (Some(_), _) if template("destination").is_some() => template("destination"),
        (_, Some(_)) if template("exit").is_some() => template("exit"),
        _ if !way_name.is_empty() && template("name").is_some() => template("name"),
        _ if non_empty(options.waypoint_name).is_some() && template("named").is_some() => template("named"),
        _ => template("default"),
    }
    .unwrap_or("");

    let first_destination = destinations.map(first_destination).unwrap_or_default();
    let nth = match options.leg_index {
        Some(index) if options.leg_count.is_none_or(|count| index + 1 != count) => ordinalize(index + 1),
        _ => String::new(),
    };
    let exit_first = step.exits.as_deref().unwrap_or("").split(';').next().unwrap_or("").to_string();
    let exit_number = ordinalize(step.maneuver.exit.filter(|e| *e != 0).unwrap_or(1) as usize);
    let modifier_text = modifier.and_then(|m| v5["constants"]["modifier"].get(m)).and_then(Value::as_str);
    let direction = direction_from_degree(step.maneuver.bearing_after);
    tokenize(instruction, &|tag| match tag {
        "way_name" => Some(way_name.clone()),
        "destination" => Some(first_destination.clone()),
        "exit" => Some(exit_first.clone()),
        "exit_number" => Some(exit_number.clone()),
        "rotary_name" => step.rotary_name.clone(),
        "lane_instruction" => lane_instruction.clone(),
        "modifier" => modifier_text.map(str::to_string),
        "direction" => Some(direction.to_string()),
        "nth" => Some(nth.clone()),
        "waypoint_name" => options.waypoint_name.map(str::to_string),
        _ => None,
    })
}

pub fn way_name(step: &OsrmStep, classes: &[String]) -> String {
    let raw_ref = step.reference.as_deref();
    let mut name = step.name.clone().unwrap_or_default();
    let reference = raw_ref.unwrap_or("").split(';').next().unwrap_or("").to_string();
    if Some(name.as_str()) == raw_ref {
        name.clear();
    }
    name = name.replacen(&format!(" ({})", raw_ref.unwrap_or("undefined")), "", 1);
    let motorway = classes.iter().any(|class| class == "motorway");
    if !name.is_empty() && !reference.is_empty() && name != reference && !motorway {
        let phrase = v5()["phrase"]["name and ref"].as_str().unwrap_or("{name} ({ref})");
        tokenize(phrase, &|tag| match tag {
            "name" => Some(name.clone()),
            "ref" => Some(reference.clone()),
            _ => None,
        })
    } else if (!name.is_empty() && !reference.is_empty() && motorway && reference.chars().any(|c| c.is_ascii_digit()))
        || (name.is_empty() && !reference.is_empty())
    {
        reference
    } else {
        name
    }
}

fn first_destination(destinations: &str) -> String {
    let parts: Vec<&str> = destinations.split(": ").collect();
    let destination_ref = parts[0].split(',').next().unwrap_or("");
    let destination = parts.get(1).map_or("", |part| part.split(',').next().unwrap_or(""));
    if !destination.is_empty() && !destination_ref.is_empty() {
        format!("{destination_ref}: {destination}")
    } else if !destination_ref.is_empty() {
        destination_ref.to_string()
    } else {
        destination.to_string()
    }
}

fn lane_config(step: &OsrmStep) -> String {
    let mut config = String::new();
    let mut current: Option<bool> = None;
    if let Some(lanes) = step.intersections.first().and_then(|i| i.lanes.as_ref()) {
        for lane in lanes {
            if current != Some(lane.valid) {
                config.push(if lane.valid { 'o' } else { 'x' });
                current = Some(lane.valid);
            }
        }
    }
    config
}

pub fn ordinalize(number: usize) -> String {
    v5()["constants"]["ordinalize"][number.to_string()].as_str().unwrap_or("").to_string()
}

pub fn direction_from_degree(degree: Option<f64>) -> &'static str {
    let Some(d) = degree.filter(|d| d.is_finite()) else { return "" };
    let key = if (0.0..=20.0).contains(&d) {
        "north"
    } else if d > 20.0 && d < 70.0 {
        "northeast"
    } else if (70.0..=110.0).contains(&d) {
        "east"
    } else if d > 110.0 && d < 160.0 {
        "southeast"
    } else if (160.0..=200.0).contains(&d) {
        "south"
    } else if d > 200.0 && d < 250.0 {
        "southwest"
    } else if (250.0..=290.0).contains(&d) {
        "west"
    } else if d > 290.0 && d < 340.0 {
        "northwest"
    } else if (340.0..=360.0).contains(&d) {
        "north"
    } else {
        return "";
    };
    v5()["constants"]["direction"][key].as_str().unwrap_or("")
}

fn tokenize(template: &str, value_of: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut started_with_token = false;
    let mut cursor = 0usize;
    while let Some(found) = template[cursor..].find('{') {
        let open = cursor + found;
        out.push_str(&template[cursor..open]);
        match parse_token(&template[open + 1..]) {
            Some((tag, length)) => {
                let end = open + 1 + length;
                match value_of(tag) {
                    Some(value) if open == 0 => {
                        started_with_token = true;
                        out.push_str(&capitalize_first(&value));
                    }
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&template[open..end]),
                }
                cursor = end;
            }
            None => {
                out.push('{');
                cursor = open + 1;
            }
        }
    }
    out.push_str(&template[cursor..]);
    let out = out.replace("  ", " ");
    if started_with_token {
        out
    } else {
        capitalize_first(&out)
    }
}

fn parse_token(after_brace: &str) -> Option<(&str, usize)> {
    let word = |s: &str| s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    let tag_len = word(after_brace);
    if tag_len == 0 {
        return None;
    }
    let mut at = tag_len;
    if after_brace[at..].starts_with(':') {
        let grammar_len = word(&after_brace[at + 1..]);
        if grammar_len > 0 && after_brace[at + 1 + grammar_len..].starts_with('}') {
            at += 1 + grammar_len;
        }
    }
    after_brace[at..].starts_with('}').then_some((&after_brace[..tag_len], at + 1))
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `ManeuverType` from OSRM's string, through Ferrostar's own serde names.
#[must_use]
pub fn maneuver_type(kind: &str) -> Option<ManeuverType> {
    serde_json::from_value(Value::String(kind.to_string())).ok()
}

/// `ManeuverModifier` from OSRM's string, through Ferrostar's own serde names.
#[must_use]
pub fn maneuver_modifier(modifier: Option<&str>) -> Option<ManeuverModifier> {
    serde_json::from_value(Value::String(modifier?.to_string())).ok()
}

/// Each step's own sentence, in route order.
#[must_use]
pub fn sentences(route: &NavRoute) -> Vec<String> {
    let leg_count = route.legs.len();
    route
        .steps
        .iter()
        .zip(&route.step_leg)
        .map(|(step, &leg)| {
            compile(step, &CompileOptions { leg_index: Some(leg), leg_count: Some(leg_count), ..CompileOptions::default() })
        })
        .collect()
}

/// Gives every Ferrostar step WITHOUT ITS OWN instructions a sentence
/// compiled from the OSRM maneuver: `instruction`, and ONE visual
/// instruction -- the next step's maneuver, triggered for the whole of
/// this step (`trigger_distance_before_maneuver = step.distance`, so
/// Ferrostar's `get_active_visual_instruction` returns it from the first
/// metre). The last step (the arrival) shows its own sentence.
///
/// A step that already carries a host's own `visual_instructions` or
/// `spoken_instructions` -- a route built with its own `bannerInstructions`
/// / `voiceInstructions`, which Ferrostar's OSRM parser (`src/nav/
/// route_source.rs`'s `OsrmResponseParser`) already turned into these
/// fields before `attach` ever runs -- is left WHOLLY UNTOUCHED:
/// `instruction`, `visual_instructions` and `spoken_instructions` all keep
/// what the host gave them. Without this, a host whose stops carry their
/// own announcements (a transit app's bus-stop name, say) would have that
/// text silently replaced by a sentence compiled from the OSRM maneuver's
/// road name.
///
/// A compiled step whose UPCOMING step is host-supplied still banners that
/// upcoming step's OSRM-compiled sentence, not the host's text: this
/// module has no way to read a banner's `primary.text` back out as a
/// plain sentence to hand to a *different* step, and showing nothing
/// would be worse than showing the compiled one.
pub fn attach(route: &mut NavRoute) {
    let own = sentences(route);
    let count = route.route.steps.len();
    let host: Vec<bool> = route.route.steps.iter().map(host_supplied).collect();
    for index in 0..count {
        if host[index] {
            continue;
        }
        let upcoming = if index + 1 < count { index + 1 } else { index };
        let osrm = &route.steps[upcoming];
        let primary = VisualInstructionContent {
            text: own[upcoming].clone(),
            maneuver_type: maneuver_type(&osrm.maneuver.kind),
            maneuver_modifier: maneuver_modifier(osrm.maneuver.modifier.as_deref()),
            roundabout_exit_degrees: None,
            lane_info: None,
            exit_numbers: Vec::new(),
        };
        let step = &mut route.route.steps[index];
        step.instruction = own[index].clone();
        step.visual_instructions = vec![VisualInstruction {
            primary_content: primary,
            secondary_content: None,
            sub_content: None,
            trigger_distance_before_maneuver: step.distance,
        }];
    }
    attach_spoken_masked(route, &host);
}

/// Whether Ferrostar's OSRM parser already filled this step's own
/// `visual_instructions` or `spoken_instructions` from the host's
/// `bannerInstructions` / `voiceInstructions` -- in which case [`attach`]
/// leaves it alone.
fn host_supplied(step: &ferrostar::models::RouteStep) -> bool {
    !step.visual_instructions.is_empty() || !step.spoken_instructions.is_empty()
}

// ---------------------------------------------------------------------------
// Spoken instructions (`docs/superpowers/specs/2026-09-25-voice-guidance-design.md`).
//
// Ferrostar's model: each `RouteStep` carries `spoken_instructions`
// (ferrostar 0.57 `src/models.rs:379`), each with a
// `trigger_distance_before_maneuver`, and its navigator reports as current
// the LAST one whose trigger the car has passed
// (`RouteStep::get_current_spoken_instruction`, `src/models.rs:423`), so a
// step's list runs from the largest trigger to the smallest. Like the
// visual instruction, a step's spoken instructions announce the maneuver at
// its END -- the next step's.

/// The speed a trigger distance assumes when a step gives none (50 km/h).
pub const DEFAULT_SPEED_MPS: f64 = 50.0 / 3.6;
/// Early: this many seconds ahead, within these bounds...
pub const EARLY_LEAD_S: f64 = 30.0;
pub const EARLY_MIN_M: f64 = 200.0;
pub const EARLY_MAX_M: f64 = 1_500.0;
/// ...and only on a step longer than the trigger plus this.
pub const EARLY_STEP_MARGIN_M: f64 = 100.0;
/// Near: this many seconds ahead, within these bounds.
pub const NEAR_LEAD_S: f64 = 8.0;
pub const NEAR_MIN_M: f64 = 30.0;
pub const NEAR_MAX_M: f64 = 150.0;
/// A Near prompt names the maneuver after it too ("..., then turn right")
/// when that one is at most this far behind -- the banner's "Then" rule.
pub const CHAIN_MAX_M: f64 = crate::nav::view_model::THEN_HINT_MAX_M;
/// A first step shorter than its Near trigger plus this leaves no time to
/// say the Depart prompt on its own: the two become one sentence.
pub const DEPART_MERGE_M: f64 = 50.0;

/// Which prompt a spoken instruction is. Carried inside its UUID (see
/// [`spoken_kind`]), so the announcer can tell without a side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokenKind {
    Depart = 1,
    Early = 2,
    Near = 3,
    Arrival = 4,
}

/// The last five bytes of every UUID this module makes.
const ID_MARKER: [u8; 5] = *b"voice";

/// The kind of one of this module's instructions; `None` for any other.
#[must_use]
pub fn spoken_kind(instruction: &ferrostar::models::SpokenInstruction) -> Option<SpokenKind> {
    let bytes = instruction.utterance_id.as_bytes();
    if bytes[11..16] != ID_MARKER {
        return None;
    }
    match bytes[10] {
        1 => Some(SpokenKind::Depart),
        2 => Some(SpokenKind::Early),
        3 => Some(SpokenKind::Near),
        4 => Some(SpokenKind::Arrival),
        _ => None,
    }
}

/// `metres` as a voice says it, and the distance that phrase stands for:
/// 100-1000 m to the nearest 50 m, then kilometres to one decimal. Under
/// 100 m is not spoken as a distance.
#[must_use]
pub fn speech_distance(metres: f64) -> Option<(f64, String)> {
    if !metres.is_finite() || metres < 100.0 {
        return None;
    }
    let rounded = (metres / 50.0).round() * 50.0;
    if rounded < 1_000.0 {
        return Some((rounded, format!("{rounded} metres")));
    }
    let rounded = (metres / 100.0).round() * 100.0;
    let km = rounded / 1_000.0;
    let words = if (km - 1.0).abs() < 1e-9 {
        "1 kilometre".to_string()
    } else if km.fract().abs() < 1e-9 {
        format!("{km:.0} kilometres")
    } else {
        format!("{km:.1} kilometres")
    };
    Some((rounded, words))
}

/// The Early trigger at `speed_mps`: `clamp(speed x 30 s, 200 m, 1500 m)`,
/// moved to the distance its prompt says aloud.
#[must_use]
pub fn early_trigger_m(speed_mps: f64) -> f64 {
    let raw = (speed_mps * EARLY_LEAD_S).clamp(EARLY_MIN_M, EARLY_MAX_M);
    speech_distance(raw).map_or(raw, |(rounded, _)| rounded)
}

/// The Near trigger at `speed_mps`: `clamp(speed x 8 s, 30 m, 150 m)`, in whole metres.
#[must_use]
pub fn near_trigger_m(speed_mps: f64) -> f64 {
    (speed_mps * NEAR_LEAD_S).clamp(NEAR_MIN_M, NEAR_MAX_M).round()
}

/// The step's own speed (its distance over its duration), or 50 km/h.
fn step_speed_mps(step: &OsrmStep) -> f64 {
    let speed = step.distance / step.duration;
    if step.distance > 0.0 && step.duration > 0.0 && speed.is_finite() {
        speed
    } else {
        DEFAULT_SPEED_MPS
    }
}

/// A step as a voice reads it: a named road without its "(ref)", and its
/// name's abbreviated suffix said in full.
fn for_speech(step: &OsrmStep) -> OsrmStep {
    let mut step = step.clone();
    if non_empty(step.name.as_deref()).is_some() {
        step.reference = None;
    }
    step.name = step.name.as_deref().map(spoken_road_name);
    step.rotary_name = step.rotary_name.as_deref().map(spoken_road_name);
    step
}

/// Road-name suffixes a synthesizer misreads ("Victoria St" as "Victoria
/// S T", "Rd" as "R D"), and what to say instead.
const ROAD_SUFFIXES: &[(&str, &str)] = &[
    ("Rd", "Road"),
    ("St", "Street"),
    ("Ave", "Avenue"),
    ("Dr", "Drive"),
    ("Blvd", "Boulevard"),
    ("Expy", "Expressway"),
    ("Hwy", "Highway"),
];

/// `name` with its LAST word expanded when that word is a whole
/// abbreviated suffix ("Victoria St" -> "Victoria Street", "Sims Ave." ->
/// "Sims Avenue"). Only the end, and only after another word: "St Andrew's
/// Road" and "Dr Sun Yat Sen" keep their titles. A road ref ("CTE", "PIE")
/// is left as it is -- letters a synthesizer spells.
#[must_use]
pub fn spoken_road_name(name: &str) -> String {
    let trimmed = name.trim_end();
    let Some((head, last)) = trimmed.rsplit_once(' ') else {
        return name.to_string();
    };
    let bare = last.strip_suffix('.').unwrap_or(last);
    match ROAD_SUFFIXES.iter().find(|(short, _)| *short == bare) {
        Some((_, long)) if !head.trim().is_empty() => format!("{head} {long}"),
        _ => name.to_string(),
    }
}

/// The pause after a distance lead-in, in the SSML rendering.
pub const LEAD_IN_BREAK: &str = "150ms";
/// The SSML rendering's rate: the plain-text shells set 0.95 on the
/// utterance, which a synthesizer given markup ignores.
pub const SSML_RATE: &str = "95%";

/// A prompt as said: an optional distance lead-in ("In 200 metres") and
/// the maneuver ("turn left onto Middle Road"), as a plain sentence
/// ("In 200 metres, turn left onto Middle Road.") and as SSML.
#[must_use]
pub fn spoken_sentence(lead: Option<&str>, body: &str) -> (String, String) {
    let body = body.trim();
    let stop = if body.ends_with(['.', '!', '?']) { "" } else { "." };
    let text = match lead {
        Some(lead) => format!("{lead}, {body}{stop}"),
        None => format!("{body}{stop}"),
    };
    let lead_ssml = lead.map_or_else(String::new, |lead| format!("{},<break time=\"{LEAD_IN_BREAK}\"/> ", xml_escape(lead)));
    let ssml = format!("<speak><prosody rate=\"{SSML_RATE}\">{lead_ssml}{}{stop}</prosody></speak>", ssml_words(body));
    (text, ssml)
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// A road ref: two to five characters, capitals and digits, at least two
/// of them capitals ("CTE", "AYE", "KPE"; not "U-turn", not "2").
fn is_road_ref(word: &str) -> bool {
    let letters = word.chars().filter(char::is_ascii_uppercase).count();
    (2..=5).contains(&word.len()) && letters >= 2 && word.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `text`, escaped, with each road ref marked to be spelt ("C T E", not
/// "cutie"): `<say-as interpret-as="characters">`.
fn ssml_words(text: &str) -> String {
    let mut out = String::new();
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let core = word.trim_end_matches([',', '.', ':', ';']);
        if is_road_ref(core) {
            out.push_str(&format!("<say-as interpret-as=\"characters\">{core}</say-as>"));
            out.push_str(&xml_escape(&word[core.len()..]));
        } else {
            out.push_str(&xml_escape(word));
        }
    }
    out
}

/// A step with no road at all, for the short "then turn right" tail.
fn unnamed(step: &OsrmStep) -> OsrmStep {
    let mut step = step.clone();
    step.name = None;
    step.reference = None;
    step.destinations = None;
    step.exits = None;
    step
}

/// "the 1st exit" -> "the first exit".
fn spoken_ordinals(mut text: String) -> String {
    const WORDS: [&str; 10] = ["first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "tenth"];
    for (index, word) in WORDS.iter().enumerate() {
        let numeral = format!(" {} exit", ordinalize(index + 1));
        if text.contains(&numeral) {
            text = text.replace(&numeral, &format!(" {word} exit"));
        }
    }
    text
}

fn decapitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

struct Speech<'a> {
    route: &'a NavRoute,
}

impl Speech<'_> {
    fn options(&self, index: usize) -> CompileOptions<'static> {
        CompileOptions { leg_index: Some(self.route.step_leg[index]), leg_count: Some(self.route.legs.len()), ..CompileOptions::default() }
    }

    fn kind(&self, index: usize) -> &str {
        self.route.steps[index].maneuver.kind.as_str()
    }

    /// Maneuver `index`'s sentence, as said at the maneuver.
    fn sentence(&self, index: usize) -> String {
        spoken_ordinals(compile(&for_speech(&self.route.steps[index]), &self.options(index)))
    }

    /// The table's own "upcoming" arrival: "You will arrive at your destination, on the left".
    fn arrive_upcoming(&self, index: usize) -> String {
        let step = &self.route.steps[index];
        let arrive = &v5()["arrive"];
        let object = non_empty(step.maneuver.modifier.as_deref()).and_then(|m| arrive.get(m)).unwrap_or(&arrive["default"]);
        let template = object["upcoming"].as_str().unwrap_or("You will arrive at your {nth} destination");
        let options = self.options(index);
        let nth = match options.leg_index {
            Some(leg) if options.leg_count.is_none_or(|count| leg + 1 != count) => ordinalize(leg + 1),
            _ => String::new(),
        };
        tokenize(template, &|tag| (tag == "nth").then(|| nth.clone()))
    }

    /// Maneuver `index` said ahead of time, after "In 400 metres, ".
    fn ahead(&self, index: usize) -> String {
        decapitalize(&if self.kind(index) == "arrive" { self.arrive_upcoming(index) } else { self.sentence(index) })
    }

    /// Maneuver `index` as the short tail of a Near prompt: no road name.
    fn tail(&self, index: usize) -> String {
        if self.kind(index) == "arrive" {
            return decapitalize(&self.arrive_upcoming(index));
        }
        decapitalize(&spoken_ordinals(compile(&unnamed(&self.route.steps[index]), &self.options(index))))
    }

    /// Whether maneuver `index` gets prompts of its own: not the exit of a
    /// roundabout whose entry prompt already named that exit.
    fn announced(&self, index: usize) -> bool {
        let exit_of_roundabout = matches!(self.kind(index), "exit rotary" | "exit roundabout")
            && index > 0
            && matches!(self.kind(index - 1), "rotary" | "roundabout");
        !exit_of_roundabout
    }

    /// The next announced maneuver after `index`, and how far beyond
    /// maneuver `index` it is.
    fn next_announced(&self, index: usize) -> Option<(usize, f64)> {
        let mut distance = 0.0;
        for next in index + 1..self.route.steps.len() {
            distance += self.route.steps[next - 1].distance;
            if self.announced(next) {
                return Some((next, distance));
            }
        }
        None
    }

    /// The Near prompt for maneuver `index`, chained when the one after is
    /// close -- and which maneuver that chained tail named.
    fn near(&self, index: usize) -> (String, Option<usize>) {
        let text = self.sentence(index);
        match self.next_announced(index) {
            Some((next, distance)) if distance <= CHAIN_MAX_M => (format!("{text}, then {}", self.tail(next)), Some(next)),
            _ => (text, None),
        }
    }

    /// Early is for a turn: a road that only changes its name ("Continue
    /// onto Sheares Avenue") gets its Near alone.
    fn worth_an_early(&self, index: usize) -> bool {
        let step = &self.route.steps[index];
        let straight_on = matches!(step.maneuver.modifier.as_deref(), None | Some("straight"));
        !(self.kind(index) == "new name" || (self.kind(index) == "continue" && straight_on))
    }
}

/// A deterministic UUID (version 8, RFC 9562's "custom"): the route's
/// geometry hash, the step, the kind, and [`ID_MARKER`]. Built through
/// Ferrostar's own serde, so `src/nav` needs no `uuid` dependency of its own.
fn instruction(route_key: u64, step: usize, kind: SpokenKind, trigger_m: f64, lead: Option<&str>, body: &str) -> ferrostar::models::SpokenInstruction {
    let (text, ssml) = spoken_sentence(lead, body);
    let marker = u64::from_be_bytes([0, 0, 0, ID_MARKER[0], ID_MARKER[1], ID_MARKER[2], ID_MARKER[3], ID_MARKER[4]]);
    let node = (u64::from(kind as u8) << 40) | marker;
    let id = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (route_key >> 32) as u32,
        (route_key >> 16) & 0xffff,
        0x8000 | (route_key & 0x0fff),
        0x8000 | (step as u64 & 0x3fff),
        node
    );
    serde_json::from_value(serde_json::json!({
        "text": text,
        "ssml": ssml,
        "triggerDistanceBeforeManeuver": trigger_m,
        "utteranceId": id,
    }))
    .expect("a well-formed spoken instruction")
}

/// FNV-1a over the route's geometry and step count.
fn route_key(route: &NavRoute) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for point in &route.route.geometry {
        feed(&point.lat.to_bits().to_le_bytes());
        feed(&point.lng.to_bits().to_le_bytes());
    }
    feed(&(route.route.steps.len() as u64).to_le_bytes());
    hash
}

/// Every step's spoken instructions, largest trigger first: Depart on the
/// first step, Early and Near for the maneuver each step ends in, and the
/// arrival on the last.
///
/// A maneuver already said as the previous Near's "then ..." gets no Near
/// of its own (Organic Maps' rule for its "Then" notification,
/// `libs/routing/turns_notification_manager.cpp`): without it a run of
/// short steps says every turn twice, seconds apart. The skipped Near
/// chains nothing, so the maneuver after it is said in full -- the rule
/// alternates.
///
/// A step with its own host-supplied `visual_instructions` or
/// `spoken_instructions` (see [`attach`]'s doc comment) keeps its
/// `spoken_instructions` untouched; called on its own (not through
/// [`attach`]) this reads that straight off `route`, before anything in
/// this call has touched it.
pub fn attach_spoken(route: &mut NavRoute) {
    let host: Vec<bool> = route.route.steps.iter().map(host_supplied).collect();
    attach_spoken_masked(route, &host);
}

/// [`attach_spoken`]'s body, with `host` -- which step keeps what it
/// arrived with -- computed by the caller. [`attach`] passes in the mask
/// it took BEFORE its own loop wrote any `visual_instructions`, because by
/// the time this ran that loop would have made every compiled step's
/// `visual_instructions` non-empty too, which is indistinguishable from a
/// host's own banner if read back here.
fn attach_spoken_masked(route: &mut NavRoute, host: &[bool]) {
    let key = route_key(route);
    let count = route.steps.len().min(route.route.steps.len());
    let speech = Speech { route };
    let mut all = Vec::with_capacity(count);
    // The maneuver the last emitted Near chained as its "then".
    let mut chained: Option<usize> = None;
    for index in 0..count {
        let step = &speech.route.steps[index];
        let mut list = Vec::new();
        // This step's Near before it became a sentence, for merging Depart into it.
        let mut near_body: Option<String> = None;
        if index + 1 == count {
            list.push(instruction(key, index, SpokenKind::Arrival, 0.0, None, &speech.sentence(index)));
            all.push(list);
            continue;
        }
        let upcoming = index + 1;
        let speed = step_speed_mps(step);
        let early = early_trigger_m(speed);
        let near = near_trigger_m(speed);
        if speech.announced(upcoming) {
            if step.distance > early + EARLY_STEP_MARGIN_M && speech.worth_an_early(upcoming) {
                if let Some((_, words)) = speech_distance(early) {
                    let lead = format!("In {words}");
                    list.push(instruction(key, index, SpokenKind::Early, early, Some(&lead), &speech.ahead(upcoming)));
                }
            }
            if speech.kind(upcoming) != "arrive" {
                if chained == Some(upcoming) {
                    chained = None;
                } else {
                    let (text, tail) = speech.near(upcoming);
                    chained = tail;
                    list.push(instruction(key, index, SpokenKind::Near, near, None, &text));
                    near_body = Some(text);
                }
            }
        }
        if index == 0 {
            let depart = speech.sentence(0);
            let merge = list.last().is_some_and(|last| spoken_kind(last) == Some(SpokenKind::Near) && step.distance < near + DEPART_MERGE_M);
            if let (true, Some(near_body)) = (merge, near_body.as_deref()) {
                list.pop();
                let text = format!("{depart}, then {}", decapitalize(near_body));
                list.push(instruction(key, 0, SpokenKind::Near, step.distance, None, &text));
            } else {
                list.insert(0, instruction(key, 0, SpokenKind::Depart, step.distance, None, &depart));
            }
        }
        all.push(list);
    }
    for (index, (step, list)) in route.route.steps.iter_mut().zip(all).enumerate() {
        if !host[index] {
            step.spoken_instructions = list;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::route_source::parse_route;
    use serde::Deserialize;

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");
    /// Generated from `osrm-text-instructions/test/fixtures/v5` -- see
    /// `tests/fixtures/osrm_text_instructions/README.md`.
    const CASES: &str = include_str!("../../tests/fixtures/osrm_text_instructions/en_v5_cases.json");

    #[derive(Deserialize)]
    struct Cases {
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct Case {
        fixture: String,
        step: OsrmStep,
        #[serde(default)]
        options: CaseOptions,
        en: String,
    }

    #[derive(Deserialize, Default)]
    struct CaseOptions {
        #[serde(default, rename = "legIndex")]
        leg_index: Option<usize>,
        #[serde(default, rename = "legCount")]
        leg_count: Option<usize>,
        #[serde(default)]
        classes: Vec<String>,
        #[serde(default, rename = "waypointName")]
        waypoint_name: Option<String>,
    }

    #[test]
    fn every_english_osrm_text_instructions_fixture_compiles_to_its_sentence() {
        let cases: Cases = serde_json::from_str(CASES).expect("the vendored cases parse");
        assert_eq!(cases.cases.len(), 630, "the vendored file holds every English v5 step fixture");
        let wrong: Vec<String> = cases
            .cases
            .iter()
            .filter_map(|case| {
                let options = CompileOptions {
                    leg_index: case.options.leg_index,
                    leg_count: case.options.leg_count,
                    classes: &case.options.classes,
                    waypoint_name: case.options.waypoint_name.as_deref(),
                };
                let got = compile(&case.step, &options);
                (got != case.en).then(|| format!("{}: got {got:?}, want {:?}", case.fixture, case.en))
            })
            .collect();
        assert!(wrong.is_empty(), "{} of 630 fixtures differ:\n{}", wrong.len(), wrong.join("\n"));
    }

    #[test]
    fn the_manila_drive_reads_as_it_should() {
        let route = parse_route(MANILA).unwrap();
        assert_eq!(
            sentences(&route),
            [
                "Head south on Padre Burgos Avenue (150)",
                "Make a slight left to stay on Padre Burgos Avenue (170)",
                "Continue onto Taft Avenue (170)",
                "Make a sharp right onto Finance Road (180)",
                "Make a U-turn and continue on Finance Road (180)",
                "Turn left onto Ayala Boulevard (180)",
                "Continue onto P. Casal Street (180)",
                "Continue onto Legarda Street (180)",
                "Turn left onto J. Figueras Street",
                "Turn right onto F. Jhocson Street",
                "You have arrived at your destination, on the right",
            ]
        );
    }

    #[test]
    fn each_step_banners_the_next_maneuver_from_its_first_metre() {
        let route = parse_route(MANILA).unwrap();
        let own = sentences(&route);
        for (index, step) in route.route.steps.iter().enumerate() {
            assert_eq!(step.instruction, own[index]);
            let [visual] = step.visual_instructions.as_slice() else { panic!("step {index}: one visual instruction") };
            let upcoming = (index + 1).min(own.len() - 1);
            assert_eq!(visual.primary_content.text, own[upcoming], "step {index}");
            assert_eq!(visual.trigger_distance_before_maneuver, step.distance);
            assert!(step.get_active_visual_instruction(step.distance).is_some(), "active at the start of step {index}");
        }
        let uturn = &route.route.steps[3].visual_instructions[0].primary_content;
        assert_eq!((uturn.maneuver_type, uturn.maneuver_modifier), (Some(ManeuverType::Continue), Some(ManeuverModifier::UTurn)));
        assert_eq!(maneuver_type("roundabout turn"), Some(ManeuverType::RoundaboutTurn));
        assert_eq!(maneuver_type("teleport"), None, "an unknown type is not guessed at");
    }

    /// MANILA with `bannerInstructions` / `voiceInstructions` (and a custom
    /// `maneuver.instruction`) spliced onto the given step indices, as a
    /// host that builds its own OSRM-shaped route would -- Ferrostar's own
    /// OSRM parser turns these into `visual_instructions` /
    /// `spoken_instructions` before `attach` ever sees the route.
    fn with_host_instructions(indices: &[usize]) -> Vec<u8> {
        let mut manila: Value = serde_json::from_slice(MANILA).unwrap();
        let steps = manila["routes"][0]["legs"][0]["steps"].as_array_mut().expect("steps array");
        for &index in indices {
            let distance = steps[index]["distance"].as_f64().unwrap_or(0.0);
            steps[index]["maneuver"]["instruction"] = Value::String(format!("Host instruction {index}"));
            steps[index]["bannerInstructions"] = serde_json::json!([{
                "distanceAlongGeometry": distance,
                "primary": {
                    "text": format!("Host banner {index}: Opp Chr Community Chapel"),
                    "type": null,
                    "modifier": null,
                    "degrees": null,
                    "components": [],
                },
                "secondary": null,
                "sub": null,
            }]);
            steps[index]["voiceInstructions"] = serde_json::json!([{
                "announcement": format!("Host voice {index}: Opp Chr Community Chapel"),
                "ssmlAnnouncement": null,
                "distanceAlongGeometry": distance,
            }]);
        }
        serde_json::to_vec(&manila).unwrap()
    }

    #[test]
    fn a_route_whose_steps_all_carry_host_instructions_is_left_untouched() {
        let all: Vec<usize> = (0..11).collect();
        let bytes = with_host_instructions(&all);
        let route = parse_route(&bytes).unwrap();
        for index in all {
            let step = &route.route.steps[index];
            assert_eq!(step.instruction, format!("Host instruction {index}"), "step {index}");
            let [visual] = step.visual_instructions.as_slice() else { panic!("step {index}: one visual instruction") };
            assert_eq!(visual.primary_content.text, format!("Host banner {index}: Opp Chr Community Chapel"), "step {index}");
            let [spoken] = step.spoken_instructions.as_slice() else { panic!("step {index}: one spoken instruction") };
            assert_eq!(spoken.text, format!("Host voice {index}: Opp Chr Community Chapel"), "step {index}");
        }
    }

    #[test]
    fn a_mixed_route_leaves_the_host_step_alone_and_compiles_the_rest_as_before() {
        let bytes = with_host_instructions(&[4]);
        let route = parse_route(&bytes).unwrap();
        let own = sentences(&route);

        // The host step: wholly untouched.
        let host_step = &route.route.steps[4];
        assert_eq!(host_step.instruction, "Host instruction 4");
        assert_eq!(host_step.visual_instructions[0].primary_content.text, "Host banner 4: Opp Chr Community Chapel");
        assert_eq!(host_step.spoken_instructions[0].text, "Host voice 4: Opp Chr Community Chapel");

        // Every uninstructed step compiles exactly as it does with no host
        // instructions anywhere (own[index]/own[upcoming]) -- including
        // step 3, whose upcoming step (4) is the host step: it still
        // banners step 4's OSRM-compiled sentence, today's behaviour.
        for index in [0usize, 1, 2, 3, 5, 6, 7, 8, 9, 10] {
            let step = &route.route.steps[index];
            assert_eq!(step.instruction, own[index], "step {index}");
            let upcoming = (index + 1).min(own.len() - 1);
            assert_eq!(step.visual_instructions[0].primary_content.text, own[upcoming], "step {index}");
        }
    }

    const BUGIS: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json");
    const SELETAR: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-seletar-rotary-uturn.json");

    /// Every step's spoken instructions as `"<kind> <trigger m>: <text>"`.
    fn spoken(bytes: &[u8]) -> Vec<Vec<String>> {
        let route = parse_route(bytes).unwrap();
        route
            .route
            .steps
            .iter()
            .map(|step| {
                step.spoken_instructions
                    .iter()
                    .map(|s| format!("{:?} {}: {}", spoken_kind(s).expect("ours"), s.trigger_distance_before_maneuver, s.text))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_prompt_is_a_sentence_with_a_pause_after_its_distance() {
        let (text, ssml) = spoken_sentence(Some("In 200 metres"), "turn left onto Middle Road");
        assert_eq!(text, "In 200 metres, turn left onto Middle Road.");
        assert_eq!(
            ssml,
            "<speak><prosody rate=\"95%\">In 200 metres,<break time=\"150ms\"/> turn left onto Middle Road.</prosody></speak>"
        );
        let (text, ssml) = spoken_sentence(None, "Turn left onto Middle Road, then turn right");
        assert_eq!(text, "Turn left onto Middle Road, then turn right.");
        assert_eq!(ssml, "<speak><prosody rate=\"95%\">Turn left onto Middle Road, then turn right.</prosody></speak>");
        assert_eq!(spoken_sentence(None, "Done.").0, "Done.", "no second full stop");
    }

    #[test]
    fn ssml_escapes_its_text_and_spells_road_refs() {
        let (text, ssml) = spoken_sentence(None, "Turn right onto CTE towards \"A&B\" <Tuas>, then turn left");
        assert_eq!(text, "Turn right onto CTE towards \"A&B\" <Tuas>, then turn left.", "plain text is left alone");
        assert_eq!(
            ssml,
            "<speak><prosody rate=\"95%\">Turn right onto <say-as interpret-as=\"characters\">CTE</say-as> towards \
             &quot;A&amp;B&quot; &lt;Tuas&gt;, then turn left.</prosody></speak>"
        );
        let (_, ssml) = spoken_sentence(None, "Make a U-turn onto PIE, then turn left onto Jalan O'Connor");
        assert!(ssml.contains("<say-as interpret-as=\"characters\">PIE</say-as>, then"), "{ssml}");
        assert!(ssml.contains("U-turn") && ssml.contains("O&apos;Connor"), "{ssml}");
    }

    #[test]
    fn abbreviated_road_suffixes_are_said_in_full_only_at_the_end() {
        for (short, long) in [
            ("Victoria St", "Victoria Street"),
            ("Rochor Rd", "Rochor Road"),
            ("Sims Ave.", "Sims Avenue"),
            ("Marina Dr", "Marina Drive"),
            ("Ayala Blvd", "Ayala Boulevard"),
            ("Pan Island Expy", "Pan Island Expressway"),
            ("Pacific Coast Hwy", "Pacific Coast Highway"),
            ("St Andrew's Road", "St Andrew's Road"),
            ("Dr Sun Yat Sen Avenue", "Dr Sun Yat Sen Avenue"),
            ("Street", "Street"),
            ("St", "St"),
            ("Stadium Blvd East", "Stadium Blvd East"),
            ("CTE", "CTE"),
            ("Rdway", "Rdway"),
        ] {
            assert_eq!(spoken_road_name(short), long, "{short}");
        }
        let mut step: OsrmStep = serde_json::from_value(serde_json::json!({
            "maneuver": { "type": "turn", "modifier": "left", "location": [103.85, 1.30] },
            "name": "Rochor Rd", "distance": 0.0, "duration": 0.0, "intersections": []
        }))
        .unwrap();
        assert_eq!(compile(&for_speech(&step), &CompileOptions::default()), "Turn left onto Rochor Road");
        step.name = Some(String::new());
        step.reference = Some("CTE".into());
        assert_eq!(compile(&for_speech(&step), &CompileOptions::default()), "Turn left onto CTE", "a ref keeps its letters");
    }

    #[test]
    fn speech_distances_are_rounded_for_the_ear() {
        assert_eq!(speech_distance(99.0), None, "under 100 m is not spoken as a distance");
        assert_eq!(speech_distance(100.0), Some((100.0, "100 metres".to_string())));
        assert_eq!(speech_distance(417.0), Some((400.0, "400 metres".to_string())));
        assert_eq!(speech_distance(482.0), Some((500.0, "500 metres".to_string())));
        assert_eq!(speech_distance(990.0), Some((1000.0, "1 kilometre".to_string())));
        assert_eq!(speech_distance(1_240.0), Some((1200.0, "1.2 kilometres".to_string())));
        assert_eq!(speech_distance(1_500.0), Some((1500.0, "1.5 kilometres".to_string())));
        assert_eq!(speech_distance(2_000.0), Some((2000.0, "2 kilometres".to_string())));
    }

    #[test]
    fn the_triggers_follow_the_spec_formulas() {
        let at = |kmh: f64| kmh / 3.6;
        assert_eq!(early_trigger_m(at(50.0)), 400.0, "13.9 m/s x 30 s = 417 m, spoken as 400");
        assert_eq!(early_trigger_m(at(10.0)), 200.0, "never under 200 m");
        assert_eq!(early_trigger_m(at(300.0)), 1500.0, "never over 1500 m");
        assert_eq!(near_trigger_m(at(50.0)), 111.0);
        assert_eq!(near_trigger_m(at(5.0)), 30.0);
        assert_eq!(near_trigger_m(at(120.0)), 150.0);
    }

    const MANILA_SPOKEN: &[&[&str]] = &[
        &[
            "Depart 171.7: Head south on Padre Burgos Avenue.",
            "Near 69: Make a slight left to stay on Padre Burgos Avenue.",
        ],
        &[
            "Near 70: Continue onto Taft Avenue.",
        ],
        &[
            "Early 200: In 200 metres, make a sharp right onto Finance Road.",
            "Near 58: Make a sharp right onto Finance Road, then make a U-turn.",
        ],
        &[],
        &[
            "Near 34: Turn left onto Ayala Boulevard.",
        ],
        &[
            "Near 44: Continue onto P. Casal Street.",
        ],
        &[
            "Near 46: Continue onto Legarda Street.",
        ],
        &[
            "Early 200: In 200 metres, turn left onto J. Figueras Street.",
            "Near 42: Turn left onto J. Figueras Street.",
        ],
        &[
            "Near 36: Turn right onto F. Jhocson Street, then you will arrive at your destination, on the right.",
        ],
        &[],
        &[
            "Arrival 0: You have arrived at your destination, on the right.",
        ],
    ];

    const BUGIS_SPOKEN: &[&[&str]] = &[
        &[
            "Depart 303.1: Head southwest on Victoria Street.",
            "Early 200: In 200 metres, turn left onto Middle Road.",
            "Near 49: Turn left onto Middle Road.",
        ],
        &[
            "Early 200: In 200 metres, turn left onto Beach Road.",
            "Near 47: Turn left onto Beach Road.",
        ],
        &[
            "Early 200: In 200 metres, turn right onto Ophir Road.",
            "Near 44: Turn right onto Ophir Road.",
        ],
        &[
            "Early 300: In 300 metres, keep right onto Ophir Road.",
            "Near 84: Keep right onto Ophir Road.",
        ],
        &[
            "Near 112: Continue onto Sheares Avenue.",
        ],
        &[
            "Early 500: In 500 metres, make a slight right onto Sheares Link.",
            "Near 129: Make a slight right onto Sheares Link, then turn right.",
        ],
        &[],
        &[
            "Near 42: Turn right, then turn left.",
        ],
        &[],
        &[
            "Near 38: Turn left onto Sheares Avenue, then turn left.",
        ],
        &[],
        &[],
        &[
            "Arrival 0: You have arrived at your destination, on the left.",
        ],
    ];

    const SELETAR_SPOKEN: &[&[&str]] = &[
        &[
            "Near 43.7: Head east on Brompton Road, then turn right onto Maida Vale.",
        ],
        &[
            "Near 50: Enter Piccadilly Circus and take the first exit onto Piccadilly.",
        ],
        &[],
        &[
            "Early 250: In 250 metres, go straight onto Seletar Aerospace Drive.",
            "Near 69: Go straight onto Seletar Aerospace Drive.",
        ],
        &[
            "Early 350: In 350 metres, make a U-turn and continue on Seletar Aerospace Drive.",
            "Near 87: Make a U-turn and continue on Seletar Aerospace Drive.",
        ],
        &[
            "Near 58: Turn left, then turn right.",
        ],
        &[],
        &[
            "Early 200: In 200 metres, turn left.",
            "Near 30: Turn left, then turn right.",
        ],
        &[],
        &[],
        &[
            "Arrival 0: You have arrived at your destination, on the left.",
        ],
    ];

    fn assert_spoken(bytes: &[u8], want: &[&[&str]]) {
        let got = spoken(bytes);
        assert_eq!(got.len(), want.len());
        for (index, (got, want)) in got.iter().zip(want).enumerate() {
            assert_eq!(got, want, "step {index}");
        }
    }

    /// Depart; Early only on a step long enough to say it, and not for a
    /// name change; Near on every maneuver, chained where the next follows
    /// within 150 m -- and no Near for a maneuver already said as a "then";
    /// the arrival.
    #[test]
    fn the_bugis_drive_speaks_early_near_chained_and_arrival() {
        assert_spoken(BUGIS, BUGIS_SPOKEN);
    }

    /// The sharp right chains the U-turn 82 m after it; references ("(180)")
    /// are not read out.
    #[test]
    fn the_manila_drive_speaks_its_u_turn_chain_without_road_numbers() {
        assert_spoken(MANILA, MANILA_SPOKEN);
    }

    /// A 44 m first step merges Depart into the first Near; the rotary's
    /// exit ordinal is a word; its own exit step says nothing.
    #[test]
    fn the_seletar_drive_speaks_its_rotary_and_u_turn() {
        assert_spoken(SELETAR, SELETAR_SPOKEN);
    }

    #[test]
    fn a_roundabout_speaks_its_exit_ordinal_and_its_exit_is_not_spoken_twice() {
        let got = spoken(SELETAR);
        assert!(got[1].iter().any(|s| s.ends_with("Enter Piccadilly Circus and take the first exit onto Piccadilly.")), "{:?}", got[1]);
        assert!(got[2].is_empty(), "the exit the roundabout prompt already named: {:?}", got[2]);
    }

    #[test]
    fn every_utterance_id_is_deterministic_and_distinct() {
        let ids = |bytes: &[u8]| -> Vec<String> {
            parse_route(bytes).unwrap().route.steps.iter().flat_map(|s| s.spoken_instructions.iter().map(|i| i.utterance_id.to_string())).collect()
        };
        let first = ids(MANILA);
        assert_eq!(first, ids(MANILA), "the same route, the same ids");
        let mut unique = first.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), first.len());
        assert!(ids(BUGIS).iter().all(|id| !first.contains(id)), "another route, other ids");
    }

    #[test]
    fn a_roundabout_exit_and_a_left_hand_uturn_are_phrased_from_the_table() {
        let step = |json: &str| -> OsrmStep { serde_json::from_str(json).unwrap() };
        let options = CompileOptions::default();
        assert_eq!(
            compile(&step(r#"{"maneuver":{"type":"roundabout","modifier":"left","exit":3},"name":"Orchard Road"}"#), &options),
            "Enter the roundabout and take the 3rd exit onto Orchard Road"
        );
        assert_eq!(
            compile(&step(r#"{"maneuver":{"type":"continue","modifier":"uturn"},"name":"","driving_side":"left"}"#), &options),
            "Make a U-turn"
        );
        assert_eq!(compile(&step(r#"{"maneuver":{"type":"turn","modifier":"left"},"name":""}"#), &options), "Turn left", "an unnamed road falls back, never invents a name");
    }
}
