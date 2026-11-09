//! What to say, and when: the announcer every shell shares
//! (`docs/superpowers/specs/2026-09-25-voice-guidance-design.md`).
//!
//! Ferrostar reports the step's current spoken instruction in its trip
//! state (`TripState::Navigating::spoken_instruction`, ferrostar 0.57
//! `src/navigation_controller/models.rs:242`, filled from
//! `RouteStep::get_current_spoken_instruction`, `src/models.rs:423`), and
//! withholds it while completely off the route
//! (`src/navigation_controller/mod.rs:403`). Its iOS and Android apps speak
//! when that instruction's UUID changes; [`VoiceAnnouncer`] is that rule
//! plus the ones the spec adds -- once per trip, silence while paused,
//! off-route, rerouting or arrived, no second Depart after a reroute, no
//! stale Early, and mute. A shell turns the [`Utterance`] into sound and
//! decides nothing.
//!
//! Pure: the trip state, time and position come in; nothing is read.

use crate::nav::instructions::{spoken_kind, SpokenKind};
use crate::nav::trip::NavTrip;
use crate::nav::view_model::TripPhase;
use ferrostar::models::SpokenInstruction;
use ferrostar::navigation_controller::models::TripState;
use std::collections::BTreeSet;

/// An Early prompt ("In 400 metres, ...") first seen with the car already
/// nearer than this share of its trigger would say a wrong distance: after
/// a seek, a stall or at 8x. It is dropped; the Near prompt follows.
pub const EARLY_STALE_FRACTION: f64 = 0.75;

/// One thing to say. A new one interrupts the one in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    /// The instruction's UUID, hyphenated.
    pub id: String,
    /// A full sentence, for a synthesizer that takes plain text.
    pub text: String,
    /// The same sentence as SSML (`<speak>`, a short pause after a
    /// distance lead-in), for a synthesizer that takes markup. `None` for a
    /// prompt that did not come from `nav::instructions`.
    pub ssml: Option<String>,
}

/// What a shell's speaker is told to do (`NavHost::take_voice`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommand {
    /// Say this next: the Apple speaker lets the prompt playing finish
    /// when nothing else waits (`speech::speak_queues`), else interrupts.
    Speak(Utterance),
    /// Silence: mute turned on, the trip ended, or a reroute started.
    Stop,
}

/// One voice a platform offers, as [`pick_voice`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceInfo {
    /// The display name: "Samantha", "Ava (Premium)", "Google US English".
    pub name: String,
    /// BCP 47, either separator: "en-US", "en_GB".
    pub lang: String,
    /// Apple's `AVSpeechSynthesisVoiceQuality`: 1 default, 2 Enhanced,
    /// 3 Premium. `None` where the platform does not say (the browser).
    pub quality: Option<u8>,
    /// Synthesized on the device, not by a network service.
    pub local: bool,
}

/// Voices that read a prompt well: a name that is, or starts with, one of
/// these (then a word boundary) is preferred.
const PREFERRED_VOICES: &[&str] =
    &["Google US English", "Google UK English Female", "Samantha", "Ava", "Zoe", "Evan", "Nathan", "Siri"];
/// A name containing one of these is a neural or downloaded voice.
const FINE_VOICE_MARKS: &[&str] = &["natural", "neural", "premium", "enhanced"];
/// Apple's novelty voices, the old compact ones, and eSpeak: they read a
/// prompt as a robot, a bell or a 1990s answering machine. Picked only if
/// nothing else speaks English.
const NOVELTY_VOICES: &[&str] = &[
    "Albert", "Bad News", "Bahh", "Bells", "Boing", "Bubbles", "Cellos", "Good News", "Jester", "Organ", "Superstar",
    "Trinoids", "Whisper", "Wobble", "Zarvox", "Fred", "Junior", "Ralph", "Kathy", "Grandma", "Grandpa", "Eddy", "Flo",
    "Reed", "Rocko", "Sandy", "Shelley",
];

/// `name` is `word`, or starts with it and then a non-letter ("Eddy
/// (English (US))", "Samantha (Enhanced)"), ignoring case.
fn name_is(name: &str, word: &str) -> bool {
    let (name, word) = (name.to_lowercase(), word.to_lowercase());
    name.strip_prefix(&word).is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
}

/// The ranking key [`pick_voice`] maximises; `None` for a voice that does
/// not speak English.
fn voice_rank(voice: &VoiceInfo) -> Option<(bool, u8, u8, u8, bool)> {
    let lang = voice.lang.replace('_', "-").to_lowercase();
    let language = if lang == "en-us" || lang == "en-gb" {
        2
    } else if lang == "en" || lang.starts_with("en-") {
        1
    } else {
        return None;
    };
    let lower = voice.name.to_lowercase();
    let novelty = NOVELTY_VOICES.iter().any(|n| name_is(&voice.name, n)) || lower.contains("espeak");
    // Chrome's own voices, network-backed, read better than the macOS
    // default-quality voices Chrome lists beside them.
    let name = if FINE_VOICE_MARKS.iter().any(|m| lower.contains(m)) {
        3
    } else if lower.starts_with("google") && PREFERRED_VOICES.iter().any(|n| name_is(&voice.name, n)) {
        2
    } else if PREFERRED_VOICES.iter().any(|n| name_is(&voice.name, n)) {
        1
    } else {
        0
    };
    Some((!novelty, language, voice.quality.unwrap_or(0), name, voice.local))
}

/// The voice to read the (English) prompts in, as an index into `voices`;
/// `None` when none speaks English, so the platform default is left alone.
///
/// Ranked, most important first: not a novelty or compact voice; `en-US`
/// or `en-GB`, then any other English; Apple's Premium, then Enhanced,
/// then default quality; a neural or known-good name; on the device. The
/// first of equals wins, so the platform's own order breaks ties.
#[must_use]
pub fn pick_voice(voices: &[VoiceInfo]) -> Option<usize> {
    let mut best: Option<(usize, (bool, u8, u8, u8, bool))> = None;
    for (index, voice) in voices.iter().enumerate() {
        if let Some(key) = voice_rank(voice) {
            if best.as_ref().is_none_or(|(_, top)| key > *top) {
                best = Some((index, key));
            }
        }
    }
    best.map(|(index, _)| index)
}

/// The voice to read text in `lang` (BCP 47, either separator: "ja-JP",
/// "zh_CN", or a bare "th") with, as an index into `voices`; `None` when
/// none speaks it. A voice whose whole tag matches outranks one that only
/// shares the language; then the [`pick_voice`] order (not a novelty voice,
/// Premium, Enhanced, a known-good name, on the device). English text gets
/// exactly [`pick_voice`]'s choice.
#[must_use]
pub fn pick_voice_for(voices: &[VoiceInfo], lang: &str) -> Option<usize> {
    let want = lang.replace('_', "-").to_lowercase();
    let primary = want.split('-').next().unwrap_or("").to_string();
    if primary == "en" {
        return pick_voice(voices);
    }
    let mut best: Option<(usize, (bool, u8, u8, u8, bool))> = None;
    for (index, voice) in voices.iter().enumerate() {
        let have = voice.lang.replace('_', "-").to_lowercase();
        let language = if have == want {
            2
        } else if have.split('-').next() == Some(primary.as_str()) {
            1
        } else {
            continue;
        };
        let lower = voice.name.to_lowercase();
        let novelty = NOVELTY_VOICES.iter().any(|n| name_is(&voice.name, n)) || lower.contains("espeak");
        let fine = u8::from(FINE_VOICE_MARKS.iter().any(|m| lower.contains(m)));
        let key = (!novelty, language, voice.quality.unwrap_or(0), fine, voice.local);
        if best.as_ref().is_none_or(|(_, top)| key > *top) {
            best = Some((index, key));
        }
    }
    best.map(|(index, _)| index)
}

/// What the announcer looks at, one frame's worth.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceView {
    pub phase: TripPhase,
    pub paused: bool,
    /// Bumped by every reroute (`TripSession::route_generation`).
    pub route_generation: u64,
    pub distance_to_maneuver_m: f64,
    /// Ferrostar's current spoken instruction.
    pub active: Option<SpokenInstruction>,
    /// The route's arrival prompt, said once the trip has arrived.
    pub arrival: Option<SpokenInstruction>,
}

impl VoiceView {
    /// This frame's view of `trip`.
    #[must_use]
    pub fn of(trip: &NavTrip) -> Self {
        let session = trip.session();
        let (active, distance_to_maneuver_m) = match session.trip_state() {
            TripState::Navigating { spoken_instruction, progress, .. } => (spoken_instruction, progress.distance_to_next_maneuver),
            _ => (None, 0.0),
        };
        Self {
            phase: trip.state().phase,
            paused: trip.drive().is_some_and(|drive| drive.is_paused()),
            route_generation: session.route_generation(),
            distance_to_maneuver_m,
            active,
            arrival: session.route().route.steps.last().and_then(|step| step.spoken_instructions.first().cloned()),
        }
    }
}

/// Decides, frame by frame, whether to speak.
#[derive(Debug, Clone, Default)]
pub struct VoiceAnnouncer {
    muted: bool,
    /// Every instruction already said (or passed over) on this route.
    spoken: BTreeSet<String>,
    generation: Option<u64>,
}

impl VoiceAnnouncer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A new trip: nothing has been said yet. Mute carries over.
    pub fn reset(&mut self) {
        self.spoken.clear();
        self.generation = None;
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// At most one utterance for this frame.
    pub fn update(&mut self, view: &VoiceView) -> Option<Utterance> {
        if self.generation != Some(view.route_generation) {
            // A new route's instructions start fresh -- even when a replayed
            // reroute is the same route, with the same ids.
            self.spoken.clear();
            self.generation = Some(view.route_generation);
        }
        if view.paused {
            // Not consumed: said when the drive resumes, if still current.
            return None;
        }
        let candidate = match view.phase {
            TripPhase::Navigating => view.active.as_ref(),
            // The spec's "arrived-and-acknowledged" silence needs no state
            // of its own: the arrival id is said once, and acknowledging
            // (Done) ends the trip, after which nothing calls this.
            TripPhase::Arrived => view.arrival.as_ref(),
            TripPhase::OffRoute | TripPhase::Rerouting => None,
        }?;
        let id = candidate.utterance_id.to_string();
        if !self.spoken.insert(id.clone()) {
            return None;
        }
        let stale = match spoken_kind(candidate) {
            // Only a pure Depart: a first step too short for two prompts
            // merges Depart into its Near ("Head north on X, then turn
            // left onto Y"), and that is kind Near, so a reroute onto a
            // short first step still says the heading -- deliberately.
            Some(SpokenKind::Depart) => view.route_generation > 0,
            Some(SpokenKind::Early) => view.distance_to_maneuver_m < candidate.trigger_distance_before_maneuver * EARLY_STALE_FRACTION,
            _ => false,
        };
        if stale || self.muted {
            return None;
        }
        Some(Utterance { id, text: candidate.text.clone(), ssml: candidate.ssml.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::route_source::parse_route;

    const BUGIS: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json");

    /// Bugis step 0's Depart, Early and Near, and the arrival.
    fn prompts() -> (SpokenInstruction, SpokenInstruction, SpokenInstruction, SpokenInstruction) {
        let route = parse_route(BUGIS).unwrap();
        let first = &route.route.steps[0].spoken_instructions;
        let arrival = route.route.steps.last().unwrap().spoken_instructions[0].clone();
        (first[0].clone(), first[1].clone(), first[2].clone(), arrival)
    }

    fn view(active: Option<&SpokenInstruction>, distance_m: f64) -> VoiceView {
        VoiceView {
            phase: TripPhase::Navigating,
            paused: false,
            route_generation: 0,
            distance_to_maneuver_m: distance_m,
            active: active.cloned(),
            arrival: Some(prompts().3),
        }
    }

    fn said(announcer: &mut VoiceAnnouncer, view: &VoiceView) -> Option<String> {
        announcer.update(view).map(|u| u.text)
    }

    #[test]
    fn each_instruction_is_spoken_once() {
        let (depart, early, _, _) = prompts();
        let mut voice = VoiceAnnouncer::new();
        assert_eq!(said(&mut voice, &view(Some(&depart), 303.0)).as_deref(), Some("Head southwest on Victoria Street."));
        for distance in [300.0, 280.0, 250.0] {
            assert_eq!(said(&mut voice, &view(Some(&depart), distance)), None, "said once, not every frame");
        }
        let utterance = voice.update(&view(Some(&early), 204.0)).expect("the Early prompt");
        assert_eq!(utterance.text, "In 200 metres, turn left onto Middle Road.");
        assert_eq!(utterance.id, early.utterance_id.to_string());
        assert_eq!(said(&mut voice, &view(None, 150.0)), None, "nothing current, nothing said");
    }

    #[test]
    fn nothing_is_said_while_paused_off_route_or_rerouting_and_a_pause_loses_nothing() {
        let (depart, _, _, _) = prompts();
        let mut voice = VoiceAnnouncer::new();
        let paused = VoiceView { paused: true, ..view(Some(&depart), 303.0) };
        assert_eq!(said(&mut voice, &paused), None);
        for phase in [TripPhase::OffRoute, TripPhase::Rerouting] {
            assert_eq!(said(&mut voice, &VoiceView { phase, ..view(Some(&depart), 303.0) }), None, "{phase:?}");
        }
        assert_eq!(
            said(&mut voice, &view(Some(&depart), 303.0)).as_deref(),
            Some("Head southwest on Victoria Street."),
            "resumed: still current, so said"
        );
    }

    #[test]
    fn after_a_reroute_the_new_route_starts_fresh_but_says_no_second_depart() {
        let (depart, early, near, _) = prompts();
        let mut voice = VoiceAnnouncer::new();
        assert!(said(&mut voice, &view(Some(&depart), 303.0)).is_some());
        assert!(said(&mut voice, &view(Some(&early), 204.0)).is_some());
        let rerouted = |active: &SpokenInstruction, distance_m: f64| VoiceView { route_generation: 1, ..view(Some(active), distance_m) };
        assert_eq!(said(&mut voice, &rerouted(&depart, 303.0)), None, "Depart is the trip's start, not a reroute's");
        // Already inside the Near trigger: Ferrostar reports Near, and only Near is said.
        assert_eq!(said(&mut voice, &rerouted(&near, 40.0)).as_deref(), Some("Turn left onto Middle Road."));
        let mut fresh = VoiceAnnouncer::new();
        assert!(said(&mut fresh, &view(Some(&depart), 303.0)).is_some());
        assert!(said(&mut fresh, &view(Some(&early), 204.0)).is_some());
        assert_eq!(
            said(&mut fresh, &rerouted(&early, 204.0)).as_deref(),
            Some("In 200 metres, turn left onto Middle Road."),
            "the same id on a new route is new"
        );
    }

    #[test]
    fn a_skip_ahead_says_only_the_latest_and_never_a_stale_distance() {
        let (depart, early, near, _) = prompts();
        let mut voice = VoiceAnnouncer::new();
        assert!(said(&mut voice, &view(Some(&depart), 303.0)).is_some());
        // One frame jumped from 303 m to 45 m: Ferrostar's current prompt is Near.
        assert_eq!(said(&mut voice, &view(Some(&near), 45.0)).as_deref(), Some("Turn left onto Middle Road."));
        assert_eq!(said(&mut voice, &view(Some(&early), 45.0)), None, "the skipped Early does not come back");
        let mut seeked = VoiceAnnouncer::new();
        assert_eq!(said(&mut seeked, &view(Some(&early), 120.0)), None, "\"In 200 metres\" at 120 m would be wrong");
        assert_eq!(said(&mut seeked, &view(Some(&near), 45.0)).as_deref(), Some("Turn left onto Middle Road."));
    }

    #[test]
    fn mute_silences_and_what_passed_while_muted_is_not_replayed() {
        let (depart, early, near, _) = prompts();
        let mut voice = VoiceAnnouncer::new();
        assert!(!voice.muted(), "unmuted by default");
        voice.set_muted(true);
        assert!(voice.muted());
        assert_eq!(said(&mut voice, &view(Some(&depart), 303.0)), None);
        assert_eq!(said(&mut voice, &view(Some(&early), 204.0)), None);
        voice.set_muted(false);
        assert_eq!(said(&mut voice, &view(Some(&early), 190.0)), None, "unmuting does not replay");
        assert!(said(&mut voice, &view(Some(&near), 45.0)).is_some());
        voice.set_muted(true);
        voice.reset();
        assert!(voice.muted(), "a new trip keeps the mute");
    }

    #[test]
    fn arrival_is_said_once_and_then_nothing() {
        let (_, _, near, arrival) = prompts();
        let mut voice = VoiceAnnouncer::new();
        let arrived = VoiceView { phase: TripPhase::Arrived, ..view(Some(&near), 0.0) };
        assert_eq!(said(&mut voice, &arrived).as_deref(), Some("You have arrived at your destination, on the left."));
        assert_eq!(said(&mut voice, &arrived), None);
        let mut on_the_arrive_step = VoiceAnnouncer::new();
        assert!(said(&mut on_the_arrive_step, &view(Some(&arrival), 0.0)).is_some(), "Ferrostar reports it on the arrive step");
        assert_eq!(said(&mut on_the_arrive_step, &arrived), None, "not twice");
    }

    fn voices(list: &[(&str, &str, Option<u8>, bool)]) -> Vec<VoiceInfo> {
        list.iter()
            .map(|&(name, lang, quality, local)| VoiceInfo { name: name.into(), lang: lang.into(), quality, local })
            .collect()
    }

    fn picked(list: &[VoiceInfo]) -> Option<&str> {
        pick_voice(list).map(|index| list[index].name.as_str())
    }

    #[test]
    fn a_reply_is_read_by_a_voice_of_its_own_language() {
        let list = voices(&[
            ("Samantha", "en-US", Some(1), true),
            ("Kyoko", "ja-JP", Some(1), true),
            ("Kyoko (Enhanced)", "ja-JP", Some(2), true),
            ("Tingting", "zh-CN", Some(1), true),
            ("Meijia", "zh-TW", Some(1), true),
            ("Kanya", "th-TH", Some(1), true),
        ]);
        let name = |lang: &str| pick_voice_for(&list, lang).map(|i| list[i].name.as_str());
        assert_eq!(name("ja-JP"), Some("Kyoko (Enhanced)"), "the better voice of the language");
        assert_eq!(name("ja"), Some("Kyoko (Enhanced)"), "a bare language matches its regions");
        assert_eq!(name("zh_CN"), Some("Tingting"), "the whole tag outranks the language");
        assert_eq!(name("th-TH"), Some("Kanya"));
        assert_eq!(name("vi-VN"), None, "no Vietnamese voice: the platform default");
        assert_eq!(name("en-US"), picked(&list), "English is pick_voice's choice");
    }

    /// `speechSynthesis.getVoices()` in Chrome on macOS: the system's
    /// voices, novelty ones included, then Chrome's network voices.
    fn chrome_on_macos() -> Vec<VoiceInfo> {
        let mut list = voices(&[
            ("Albert", "en-US", None, true),
            ("Alice", "it-IT", None, true),
            ("Bad News", "en-US", None, true),
            ("Bahh", "en-US", None, true),
            ("Bells", "en-US", None, true),
            ("Boing", "en-US", None, true),
            ("Bubbles", "en-US", None, true),
            ("Cellos", "en-US", None, true),
            ("Daniel", "en-GB", None, true),
            ("Eddy (English (UK))", "en-GB", None, true),
            ("Eddy (English (US))", "en-US", None, true),
            ("Flo (English (US))", "en-US", None, true),
            ("Fred", "en-US", None, true),
            ("Good News", "en-US", None, true),
            ("Grandma (English (US))", "en-US", None, true),
            ("Grandpa (English (UK))", "en-GB", None, true),
            ("Jester", "en-US", None, true),
            ("Junior", "en-US", None, true),
            ("Karen", "en-AU", None, true),
            ("Kathy", "en-US", None, true),
            ("Kyoko", "ja-JP", None, true),
            ("Moira", "en-IE", None, true),
            ("Organ", "en-US", None, true),
            ("Ralph", "en-US", None, true),
            ("Reed (English (US))", "en-US", None, true),
            ("Rishi", "en-IN", None, true),
            ("Rocko (English (US))", "en-US", None, true),
            ("Samantha", "en-US", None, true),
            ("Sandy (English (US))", "en-US", None, true),
            ("Shelley (English (US))", "en-US", None, true),
            ("Superstar", "en-US", None, true),
            ("Tessa", "en-ZA", None, true),
            ("Trinoids", "en-US", None, true),
            ("Whisper", "en-US", None, true),
            ("Wobble", "en-US", None, true),
            ("Zarvox", "en-US", None, true),
        ]);
        list.extend(voices(&[
            ("Google Deutsch", "de-DE", None, false),
            ("Google US English", "en-US", None, false),
            ("Google UK English Female", "en-GB", None, false),
            ("Google UK English Male", "en-GB", None, false),
            ("Google 日本語", "ja-JP", None, false),
        ]));
        list
    }

    /// `AVSpeechSynthesisVoice.speechVoices()` on an iPhone with nothing
    /// downloaded: every voice default quality.
    fn ios_stock() -> Vec<VoiceInfo> {
        voices(&[
            ("Majed", "ar-001", Some(1), true),
            ("Albert", "en-US", Some(1), true),
            ("Arthur", "en-GB", Some(1), true),
            ("Bad News", "en-US", Some(1), true),
            ("Catherine", "en-AU", Some(1), true),
            ("Daniel", "en-GB", Some(1), true),
            ("Eddy", "en-US", Some(1), true),
            ("Fred", "en-US", Some(1), true),
            ("Gordon", "en-AU", Some(1), true),
            ("Karen", "en-AU", Some(1), true),
            ("Martha", "en-GB", Some(1), true),
            ("Moira", "en-IE", Some(1), true),
            ("Nicky", "en-US", Some(1), true),
            ("Aaron", "en-US", Some(1), true),
            ("Rishi", "en-IN", Some(1), true),
            ("Samantha", "en-US", Some(1), true),
            ("Tessa", "en-ZA", Some(1), true),
            ("Zarvox", "en-US", Some(1), true),
            ("Kyoko", "ja-JP", Some(1), true),
        ])
    }

    #[test]
    fn chrome_on_macos_picks_google_us_english_and_never_a_novelty_voice() {
        let list = chrome_on_macos();
        assert_eq!(picked(&list), Some("Google US English"));
        let local_only: Vec<_> = list.into_iter().filter(|v| v.local).collect();
        assert_eq!(picked(&local_only), Some("Samantha"), "without Chrome's own voices");
    }

    #[test]
    fn an_installed_premium_or_neural_voice_wins_in_the_browser() {
        let mut list = chrome_on_macos();
        list.extend(voices(&[("Zoe (Enhanced)", "en-US", None, true)]));
        assert_eq!(picked(&list), Some("Zoe (Enhanced)"));
        list.extend(voices(&[("Microsoft Aria Online (Natural) - English (United States)", "en-US", None, false)]));
        assert_eq!(picked(&list), Some("Zoe (Enhanced)"), "of two fine voices, the one on the device");
        let edge = voices(&[
            ("Microsoft David - English (United States)", "en-US", None, true),
            ("Microsoft Aria Online (Natural) - English (United States)", "en-US", None, false),
        ]);
        assert_eq!(picked(&edge), Some("Microsoft Aria Online (Natural) - English (United States)"));
    }

    #[test]
    fn ios_picks_samantha_stock_and_the_best_downloaded_voice_by_quality() {
        let mut list = ios_stock();
        assert_eq!(picked(&list), Some("Samantha"));
        list.extend(voices(&[("Serena", "en-GB", Some(2), true)]));
        assert_eq!(picked(&list), Some("Serena"), "Enhanced beats a preferred name at default quality");
        list.extend(voices(&[("Evan", "en-US", Some(2), true)]));
        assert_eq!(picked(&list), Some("Evan"), "of two Enhanced, the known-good name");
        list.extend(voices(&[("Ava", "en-US", Some(3), true)]));
        assert_eq!(picked(&list), Some("Ava"), "Premium beats Enhanced");
        list.extend(voices(&[("Albert", "en-US", Some(3), true)]));
        assert_eq!(picked(&list), Some("Ava"), "a novelty voice loses even at Premium");
        list.extend(voices(&[("Lee", "en-AU", Some(3), true)]));
        assert_eq!(picked(&list), Some("Ava"), "language before quality");
    }

    #[test]
    fn a_voice_list_with_no_english_leaves_the_default_alone() {
        assert_eq!(pick_voice(&[]), None);
        let list = voices(&[("Kyoko", "ja-JP", Some(1), true), ("Thomas", "fr-FR", Some(1), true)]);
        assert_eq!(pick_voice(&list), None);
        let list = voices(&[("Kyoko", "ja-JP", Some(1), true), ("Karen", "en_AU", Some(1), true)]);
        assert_eq!(picked(&list), Some("Karen"), "any English, either separator");
        let list = voices(&[("Zarvox", "en-US", None, true), ("eSpeak English", "en", None, true)]);
        assert_eq!(picked(&list), Some("Zarvox"), "only novelty English: still English");
        assert!(!name_is("Flora", "Flo") && name_is("Flo (English (US))", "Flo"), "a word, not a prefix");
    }
}

/// Whole drives through `NavHost`, the one surface a shell calls: what a
/// passenger would hear, in order.
#[cfg(test)]
mod drive_tests {
    use crate::camera_control::{CameraController, MAX_ZOOM, MIN_ZOOM};
    use crate::nav::host::{NavFrame, NavHost, SafeInsets, SimOptions, VoiceCommand, WallClock};
    use crate::nav::view_model::TripPhase;
    use crate::render::Camera;
    use std::sync::Arc;

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");
    const BUGIS: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json");
    const SELETAR: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-seletar-rotary-uturn.json");

    fn frame(now_ms: f64) -> NavFrame {
        NavFrame {
            now_ms,
            wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
            viewport: (800.0, 600.0),
            insets: SafeInsets::default(),
        }
    }

    fn camera() -> CameraController {
        CameraController::new(
            Camera { center_lon: 103.85, center_lat: 1.30, zoom: 13.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        )
    }

    /// A `Stop` in a heard list.
    const STOP: &str = "[stop]";

    /// Every command of a simulated drive, 100 ms frames, until two seconds
    /// after arrival: the text of each `Speak`, and [`STOP`].
    fn heard(route: &[u8], options: SimOptions) -> Vec<String> {
        let mut host = NavHost::new();
        host.set_demo_route(Arc::from(route), options);
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).expect("the fixture drives");
        let mut said = Vec::new();
        let mut arrived_at = None;
        for tick in 0..=20_000u32 {
            let now = f64::from(tick) * 100.0;
            let _ = host.advance(&mut camera, &frame(now));
            match host.take_voice() {
                Some(VoiceCommand::Speak(utterance)) => said.push(utterance.text),
                Some(VoiceCommand::Stop) => said.push(STOP.to_string()),
                None => {}
            }
            if host.state().is_some_and(|s| s.phase == TripPhase::Arrived) {
                let at = *arrived_at.get_or_insert(now);
                if now - at >= 2_000.0 {
                    return said;
                }
            }
        }
        panic!("never arrived; heard {said:#?}");
    }

    const BUGIS_HEARD: &[&str] = &[
        "Head southwest on Victoria Street.",
        "In 200 metres, turn left onto Middle Road.",
        "Turn left onto Middle Road.",
        "In 200 metres, turn left onto Beach Road.",
        "Turn left onto Beach Road.",
        "In 200 metres, turn right onto Ophir Road.",
        "Turn right onto Ophir Road.",
        "In 300 metres, keep right onto Ophir Road.",
        "Keep right onto Ophir Road.",
        "Continue onto Sheares Avenue.",
        "In 500 metres, make a slight right onto Sheares Link.",
        "Make a slight right onto Sheares Link, then turn right.",
        "Turn right, then turn left.",
        "Turn left onto Sheares Avenue, then turn left.",
        "You have arrived at your destination, on the left.",
    ];
    const MANILA_HEARD: &[&str] = &[
        "Head south on Padre Burgos Avenue.",
        "Make a slight left to stay on Padre Burgos Avenue.",
        "Continue onto Taft Avenue.",
        "In 200 metres, make a sharp right onto Finance Road.",
        "Make a sharp right onto Finance Road, then make a U-turn.",
        "Turn left onto Ayala Boulevard.",
        "Continue onto P. Casal Street.",
        "Continue onto Legarda Street.",
        "In 200 metres, turn left onto J. Figueras Street.",
        "Turn left onto J. Figueras Street.",
        "Turn right onto F. Jhocson Street, then you will arrive at your destination, on the right.",
        "You have arrived at your destination, on the right.",
    ];
    const SELETAR_HEARD: &[&str] = &[
        "Head east on Brompton Road, then turn right onto Maida Vale.",
        "Enter Piccadilly Circus and take the first exit onto Piccadilly.",
        "In 250 metres, go straight onto Seletar Aerospace Drive.",
        "Go straight onto Seletar Aerospace Drive.",
        "In 350 metres, make a U-turn and continue on Seletar Aerospace Drive.",
        "Make a U-turn and continue on Seletar Aerospace Drive.",
        "Turn left, then turn right.",
        "In 200 metres, turn left.",
        "Turn left, then turn right.",
        "You have arrived at your destination, on the left.",
    ];

    #[test]
    fn the_bugis_to_marina_bay_drive_is_heard_in_order() {
        assert_eq!(heard(BUGIS, SimOptions::default()), BUGIS_HEARD);
    }

    #[test]
    fn the_manila_drive_is_heard_in_order() {
        assert_eq!(heard(MANILA, SimOptions::default()), MANILA_HEARD);
    }

    #[test]
    fn the_seletar_rotary_and_u_turn_drive_is_heard_in_order() {
        assert_eq!(heard(SELETAR, SimOptions::default()), SELETAR_HEARD);
    }

    #[test]
    fn ending_the_trip_mid_prompt_stops_it_once() {
        let mut host = NavHost::new();
        host.set_demo_route(Arc::from(BUGIS), SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let _ = host.advance(&mut camera, &frame(0.0));
        assert!(matches!(host.take_voice(), Some(VoiceCommand::Speak(_))));
        host.end_trip();
        assert_eq!(host.take_voice(), Some(VoiceCommand::Stop));
        host.end_trip();
        assert_eq!(host.take_voice(), None, "once");
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let _ = host.advance(&mut camera, &frame(0.0));
        let _ = host.take_voice();
        host.set_voice_muted(true);
        host.set_voice_muted(true);
        assert_eq!(host.take_voice(), Some(VoiceCommand::Stop), "muting twice stops once");
        assert_eq!(host.take_voice(), None);
    }

    /// At 8x a tick jumps 80 m: whatever is skipped, what is heard is in
    /// the 1x order, and no "In 200 metres" is said late (the stale rule).
    #[test]
    fn at_8x_what_is_heard_is_the_1x_list_in_order_minus_what_was_skipped() {
        let fast = heard(BUGIS, SimOptions { speed: 8, ..SimOptions::default() });
        let mut rest = BUGIS_HEARD.iter();
        for said in &fast {
            assert!(rest.any(|golden| golden == said), "{said:?} out of order in {fast:#?}");
        }
        assert_eq!(fast.last().map(String::as_str), BUGIS_HEARD.last().copied(), "the arrival is still said");
    }

    #[test]
    fn a_reroute_does_not_say_depart_again() {
        // Off the route after step 5; with no live routing the demo route is
        // replayed from its start -- a new route, heard fresh, minus Depart.
        let said = heard(MANILA, SimOptions { off_route_after_step: Some(5), ..SimOptions::default() });
        assert_eq!(said.iter().filter(|s| s.as_str() == STOP).count(), 1, "the reroute stops the old route's prompt: {said:#?}");
        let departs = said.iter().filter(|s| s.starts_with("Head south")).count();
        assert_eq!(departs, 1, "{said:#?}");
        let slight_lefts = said.iter().filter(|s| s.as_str() == "Make a slight left to stay on Padre Burgos Avenue.").count();
        assert_eq!(slight_lefts, 2, "the new route's prompts are said again: {said:#?}");
    }

    #[test]
    fn the_speaker_button_mutes_through_tap_and_the_chrome_shows_it() {
        let mut host = NavHost::new();
        host.set_demo_route(Arc::from(BUGIS), SimOptions::default());
        let mut camera = camera();
        let f = frame(0.0);
        assert!(host.scene(camera.camera(), &f).buttons.is_empty(), "no trip, no toggle");
        host.start_simulation(&camera, &f).unwrap();
        let _ = host.advance(&mut camera, &f);
        assert!(matches!(host.take_voice(), Some(VoiceCommand::Speak(_))));
        let scene = host.scene(camera.camera(), &f);
        let toggle = scene.buttons.iter().find(|b| b.action == crate::nav::chrome::ChromeAction::ToggleVoice).expect("the toggle").rect;
        assert!(host.tap(toggle.x + toggle.w / 2.0, toggle.y + toggle.h / 2.0, &mut camera, &f), "the chrome takes the press");
        assert!(host.voice_muted());
        assert_eq!(host.take_voice(), Some(VoiceCommand::Stop), "the Depart prompt is cut off");
        let speaker = |host: &NavHost, camera: &CameraController| {
            host.scene(camera.camera(), &f).icons.iter().find_map(|i| match i.glyph {
                crate::nav::chrome::ChromeGlyph::Speaker { muted } => Some(muted),
                _ => None,
            })
        };
        assert_eq!(speaker(&host, &camera), Some(true));
        for tick in 1..=600u32 {
            let _ = host.advance(&mut camera, &frame(f64::from(tick) * 100.0));
            assert!(host.take_voice().is_none(), "muted at {tick}");
        }
        host.end_trip();
        assert!(host.take_voice().is_none(), "nothing was playing: no Stop");
        host.start_simulation(&camera, &f).unwrap();
        assert!(host.voice_muted(), "the mute outlives the trip");
        assert!(host.tap(toggle.x + 1.0, toggle.y + 1.0, &mut camera, &f));
        assert!(!host.voice_muted());
        assert_eq!(speaker(&host, &camera), Some(false));
    }
}
