//! The Apple speaker for turn-by-turn voice guidance: one
//! `AVSpeechSynthesizer`, shared by native macOS and the iOS shell (which
//! reaches this file by `#[path]`). Other platforms get a no-op `Speaker`
//! with the same calls, so a shell never needs a `cfg` of its own.
//!
//! This is not navigation: WHAT to say and WHEN is `nav::voice`'s, handed
//! out by `NavHost::take_voice`. A shell does exactly this each frame,
//! after `NavHost::advance`:
//!
//! ```ignore
//! self.speech.frame(self.nav.take_voice());
//! ```
//!
//! (`SpeechOutput` maps a `VoiceCommand` onto `Speaker::speak` /
//! `Speaker::stop`, then `Speaker::poll`.)
//!
//! # Never blocks the render thread
//!
//! `AVSpeechSynthesizer` is asynchronous: `speakUtterance:` queues and
//! returns. One instance lives as long as the `Speaker` -- a synthesizer
//! dropped mid-utterance stops talking, so one per prompt would be both
//! wasteful and silent.
//!
//! # iOS audio session
//!
//! The session is `.playback` with `.duckOthers`, so music dips under a
//! prompt as it does in every navigation app. Ducking lasts exactly as long
//! as the session is ACTIVE, so the session is activated when a prompt
//! starts and deactivated (with `notifyOthersOnDeactivation`, so the music
//! app knows to come back up) once the synthesizer has gone quiet. Both
//! `setActive` calls are synchronous and can take milliseconds, so they run
//! on one small worker thread, not the render thread; `poll` only asks the
//! synthesizer `isSpeaking` (one message send) while the session is up.
//! macOS has no `AVAudioSession`, and nothing to do there.

pub use imp::{voices, Speaker};

use crate::nav::host::VoiceCommand;

/// What a shell holds: a [`Speaker`] made on the first prompt, not when the
/// shell is built -- a map that never navigates never creates a synthesizer
/// -- and never made at all in a unit-test build, so a test that drives a
/// simulated trip does not make the machine talk.
#[derive(Default)]
pub struct SpeechOutput {
    speaker: Option<Speaker>,
    /// Prompts handed to the speaker so far (counted in a test build too,
    /// where nothing is said aloud): how a test sees that guidance went
    /// out while the map was not painting.
    prompts: u64,
}

impl SpeechOutput {
    /// Once a frame, after `NavHost::advance`, with `NavHost::take_voice`:
    /// `Speak` says the new prompt -- after the one playing, if one is
    /// playing and nothing is queued yet, else at once ([`speak_queues`]) --
    /// `Stop` silences everything (a mute, an end of trip), and either way
    /// the speaker releases anything it holds once quiet.
    pub fn frame(&mut self, command: Option<VoiceCommand>) {
        match command {
            Some(VoiceCommand::Speak(utterance)) => {
                self.prompts += 1;
                if cfg!(not(test)) {
                    eprintln!("[voice] say: {}", utterance.text);
                    self.speaker.get_or_insert_with(Speaker::new).say_next(&utterance.text, utterance.ssml.as_deref());
                }
            }
            // Nothing to stop if nothing was ever said.
            Some(VoiceCommand::Stop) => {
                if let Some(speaker) = self.speaker.as_mut() {
                    speaker.stop();
                }
            }
            None => {}
        }
        if let Some(speaker) = self.speaker.as_mut() {
            speaker.poll();
        }
    }
}

impl SpeechOutput {
    /// How many prompts [`Self::frame`] has handed to the speaker.
    #[must_use]
    pub fn prompts(&self) -> u64 {
        self.prompts
    }
}

/// Whether a new prompt waits for the one playing instead of cutting it
/// off: yes while one is playing and none is queued behind it yet.
///
/// Interrupting every time lost the opening prompt of a drive: the demo
/// routes' first step is short, so Ferrostar hands out the next step's
/// instruction under a second after "Head south on ...", which cut it off
/// at its second word (the speaker's own log: "the previous prompt is cut
/// off 938 ms after it was handed over"). Queueing ONE behind it keeps
/// both; a third arriving while one already waits interrupts both and
/// says only the newest, so a fast-forwarded drive never builds a backlog
/// of stale distances. Pure, so it is tested off Apple too.
#[must_use]
pub fn speak_queues(playing: bool, queued: bool) -> bool {
    playing && !queued
}

#[cfg(target_vendor = "apple")]
mod imp {
    use crate::nav::host::{pick_voice, pick_voice_for, VoiceInfo};
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2::{sel, AnyThread, ClassType};
    use objc2_avf_audio::{
        AVSpeechBoundary, AVSpeechSynthesisVoice, AVSpeechSynthesizer, AVSpeechUtterance,
        AVSpeechUtteranceDefaultSpeechRate,
    };
    use objc2_foundation::NSString;

    /// The fallback voice when [`pick_voice`] finds none. The instructions are English.
    const LANGUAGE: &str = "en-US";
    /// A touch slower than Apple's default rate, which hurries a road name.
    /// (An SSML utterance ignores `rate`; its markup carries the same 95%.)
    const RATE_SCALE: f32 = 0.95;

    pub struct Speaker {
        synth: Retained<AVSpeechSynthesizer>,
        /// `None` if the system has no English voice at all, in which case
        /// the utterance falls back to the system default voice.
        voice: Option<Retained<AVSpeechSynthesisVoice>>,
        /// `AVSpeechUtterance` answers `initWithSSMLRepresentation:` (iOS 16,
        /// macOS 13 and later).
        ssml: bool,
        #[cfg(target_os = "ios")]
        session: session::Session,
        /// When the last prompt was handed over and whether the synthesizer
        /// has been seen speaking it since: a prompt it never starts is
        /// logged by [`Self::poll`], never silently dropped.
        pending: Option<(std::time::Instant, bool)>,
        /// A prompt is waiting behind the one playing ([`super::speak_queues`]).
        queued: bool,
    }

    /// A prompt the synthesizer has not started this long after `speak` is
    /// reported as failed.
    const START_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1_500);

    /// Every voice installed, as `pick_voice` sees them.
    fn installed_voices() -> Vec<(Retained<AVSpeechSynthesisVoice>, VoiceInfo)> {
        // SAFETY: a class method and property reads, no preconditions.
        unsafe {
            AVSpeechSynthesisVoice::speechVoices()
                .to_vec()
                .into_iter()
                .map(|voice| {
                    let quality = u8::try_from(voice.quality().0).ok();
                    let info = VoiceInfo {
                        name: voice.name().to_string(),
                        lang: voice.language().to_string(),
                        quality,
                        local: true,
                    };
                    (voice, info)
                })
                .collect()
        }
    }

    /// The voice [`pick_voice`] ranks best, else the system's `en-US` voice.
    fn best_voice() -> Option<Retained<AVSpeechSynthesisVoice>> {
        let mut voices = installed_voices();
        let infos: Vec<VoiceInfo> = voices.iter().map(|(_, info)| info.clone()).collect();
        match pick_voice(&infos) {
            Some(index) => {
                eprintln!("[voice] using: {}", infos[index].name);
                Some(voices.swap_remove(index).0)
            }
            None => {
                // SAFETY: a class method, no preconditions.
                let voice = unsafe { AVSpeechSynthesisVoice::voiceWithLanguage(Some(&NSString::from_str(LANGUAGE))) };
                eprintln!("[voice] using: {}", voice.as_ref().map_or("the system default".to_string(), |v| unsafe { v.name() }.to_string()));
                voice
            }
        }
    }

    /// The installed voices, as the platform lists them.
    #[must_use]
    pub fn voices() -> Vec<VoiceInfo> {
        installed_voices().into_iter().map(|(_, info)| info).collect()
    }

    impl Speaker {
        #[must_use]
        pub fn new() -> Self {
            // SAFETY: plain constructors/class methods with no preconditions;
            // neither class is main-thread-only.
            let synth = unsafe { AVSpeechSynthesizer::new() };
            let class: &AnyClass = AVSpeechUtterance::class();
            Self {
                synth,
                voice: best_voice(),
                ssml: class.responds_to(sel!(initWithSSMLRepresentation:)),
                #[cfg(target_os = "ios")]
                session: session::Session::default(),
                pending: None,
                queued: false,
            }
        }

        /// Reads from now on in `lang` (BCP 47): the installed voice
        /// named `name` if it speaks that language, else the best one that
        /// does ([`pick_voice_for`]), else the platform default for the
        /// tag. Answers the voice's name. Studio's spoken chat replies use
        /// it; turn-by-turn keeps [`Speaker::new`]'s English pick.
        pub fn use_voice(&mut self, lang: &str, name: Option<&str>) -> Option<String> {
            let mut voices = installed_voices();
            let primary = |tag: &str| tag.replace('_', "-").to_lowercase().split('-').next().unwrap_or("").to_string();
            let named = name.and_then(|n| voices.iter().position(|(_, info)| info.name == n && primary(&info.lang) == primary(lang)));
            let infos: Vec<VoiceInfo> = voices.iter().map(|(_, info)| info.clone()).collect();
            self.voice = match named.or_else(|| pick_voice_for(&infos, lang)) {
                Some(index) => Some(voices.swap_remove(index).0),
                // SAFETY: a class method taking an NSString; no preconditions.
                None => unsafe { AVSpeechSynthesisVoice::voiceWithLanguage(Some(&NSString::from_str(lang))) },
            };
            self.voice_name()
        }

        /// A prompt is playing, or handed over and not yet started (it
        /// polls first).
        pub fn is_busy(&mut self) -> bool {
            self.poll();
            // SAFETY: a property read on our own synthesizer.
            self.pending.is_some() || unsafe { self.synth.isSpeaking() }
        }

        /// The chosen voice's name, `None` for the system default.
        #[must_use]
        pub fn voice_name(&self) -> Option<String> {
            // SAFETY: a property read on a live object.
            self.voice.as_ref().map(|voice| unsafe { voice.name() }.to_string())
        }

        /// Say `text`, interrupting whatever is being said. Returns at once.
        pub fn speak(&mut self, text: &str) {
            self.speak_marked_up(text, None);
        }

        /// Say `text` (as [`Self::speak_marked_up`] reads it) after the
        /// prompt playing when [`super::speak_queues`] says so, else at once,
        /// interrupting. Returns at once.
        pub fn say_next(&mut self, text: &str, ssml: Option<&str>) {
            if text.trim().is_empty() {
                return;
            }
            // SAFETY: a property read on a live object.
            let playing = unsafe { self.synth.isSpeaking() };
            if super::speak_queues(playing, self.queued) {
                self.queued = true;
                self.enqueue(text, ssml);
            } else {
                self.speak_marked_up(text, ssml);
            }
        }

        /// Say `text` -- as `ssml` where the platform reads SSML and the
        /// markup parses, else as plain text -- interrupting whatever is
        /// being said. Returns at once.
        pub fn speak_marked_up(&mut self, text: &str, ssml: Option<&str>) {
            if text.trim().is_empty() {
                return;
            }
            self.queued = false;
            if let Some((since, started)) = self.pending {
                eprintln!(
                    "[voice] the previous prompt is cut off {} ms after it was handed over ({})",
                    since.elapsed().as_millis(),
                    if started { "it had started" } else { "before a word of it was heard" }
                );
            }
            // SAFETY: `synth` and the utterance are live, retained objects;
            // `stopSpeakingAtBoundary:` on an idle synthesizer is a no-op
            // that returns NO; `initWithSSMLRepresentation:` is only sent
            // where `new` found the class answers it, and returns nil for
            // markup it rejects.
            unsafe {
                let _ = self.synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);
            }
            self.enqueue(text, ssml);
        }

        /// Hands `text` to the synthesizer's own queue, behind anything it
        /// is saying.
        fn enqueue(&mut self, text: &str, ssml: Option<&str>) {
            #[cfg(target_os = "ios")]
            self.session.activate();
            // SAFETY: as in `speak_marked_up`.
            unsafe {
                let marked_up = ssml.filter(|_| self.ssml).and_then(|ssml| {
                    AVSpeechUtterance::initWithSSMLRepresentation(AVSpeechUtterance::alloc(), &NSString::from_str(ssml))
                });
                let utterance = marked_up.unwrap_or_else(|| {
                    let plain = AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(text));
                    plain.setRate(AVSpeechUtteranceDefaultSpeechRate * RATE_SCALE);
                    plain.setPitchMultiplier(1.0);
                    plain
                });
                utterance.setVoice(self.voice.as_deref());
                self.synth.speakUtterance(&utterance);
            }
            self.pending = Some((std::time::Instant::now(), false));
        }

        /// Silence whatever is being said, at once. On iOS the audio session
        /// is released too, so ducked music comes straight back up.
        pub fn stop(&mut self) {
            // SAFETY: as in `speak`.
            let _ = unsafe { self.synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
            self.pending = None;
            self.queued = false;
            #[cfg(target_os = "ios")]
            self.session.deactivate();
        }

        /// Call once a frame. Logs a prompt the synthesizer never started
        /// (one `isSpeaking` read a frame, only while a prompt is pending).
        /// On iOS, also releases the audio session (so ducked music comes
        /// back up) once the synthesizer has gone quiet; free while nothing
        /// has been said.
        pub fn poll(&mut self) {
            if let Some((since, started)) = self.pending {
                // SAFETY: a property read on a live object.
                if unsafe { self.synth.isSpeaking() } {
                    self.pending = Some((since, true));
                } else if started {
                    self.pending = None;
                    self.queued = false;
                } else if since.elapsed() >= START_TIMEOUT {
                    eprintln!(
                        "[voice] error: the synthesizer did not start the prompt within {} ms (voice {:?}); nothing was heard",
                        START_TIMEOUT.as_millis(),
                        self.voice_name()
                    );
                    self.pending = None;
                }
            }
            #[cfg(target_os = "ios")]
            if self.session.wants_release() {
                // SAFETY: a property read on a live object.
                let speaking = unsafe { self.synth.isSpeaking() };
                if !speaking {
                    self.session.deactivate();
                }
            }
        }
    }

    impl Default for Speaker {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Releasing the synthesizer does not silence it: the system keeps one
    /// that is mid-utterance alive until its queue is spoken, so a map
    /// dropped mid-prompt (a Studio page switch) kept talking. Stop first.
    impl Drop for Speaker {
        fn drop(&mut self) {
            self.stop();
        }
    }

    #[cfg(target_os = "ios")]
    mod session {
        use objc2_avf_audio::{
            AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayback,
            AVAudioSessionSetActiveOptions,
        };
        use std::sync::mpsc::{channel, Receiver, Sender};
        use std::time::{Duration, Instant};

        /// How long after `speak` the session is held regardless of what
        /// `isSpeaking` says: the synthesizer can report idle for a moment
        /// between `speakUtterance:` and the first sample.
        const GRACE: Duration = Duration::from_millis(750);

        /// The render thread's side: whether it has asked for the session
        /// to be up, and a channel to the worker that actually does it.
        #[derive(Default)]
        pub(super) struct Session {
            worker: Option<Sender<bool>>,
            /// When the session was last asked up; `None` while released.
            active_since: Option<Instant>,
        }

        impl Session {
            pub(super) fn activate(&mut self) {
                let first = self.active_since.is_none();
                self.active_since = Some(Instant::now());
                if first {
                    self.send(true);
                }
            }

            pub(super) fn wants_release(&self) -> bool {
                self.active_since.is_some_and(|since| since.elapsed() >= GRACE)
            }

            pub(super) fn deactivate(&mut self) {
                if self.active_since.take().is_some() {
                    self.send(false);
                }
            }

            fn send(&mut self, active: bool) {
                if self.worker.is_none() {
                    let (tx, rx) = channel();
                    let spawned = std::thread::Builder::new()
                        .name("gpui-map-audio-session".into())
                        .spawn(move || run(&rx));
                    if spawned.is_err() {
                        // Speech still works without a managed session; it
                        // just will not duck other audio.
                        return;
                    }
                    self.worker = Some(tx);
                }
                if let Some(worker) = &self.worker {
                    if worker.send(active).is_err() {
                        self.worker = None;
                    }
                }
            }
        }

        /// The worker: applies the LATEST requested state, so a burst of
        /// prompts costs one activation, not one per prompt.
        fn run(rx: &Receiver<bool>) {
            // SAFETY: `sharedInstance` has no preconditions and the session
            // is `Send + Sync`.
            let session = unsafe { AVAudioSession::sharedInstance() };
            let mut category_set = false;
            let mut is_active = false;
            while let Ok(mut want) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    want = newer;
                }
                if want == is_active {
                    continue;
                }
                // SAFETY: the category is Apple's own constant; all calls
                // are on the shared session, which is thread-safe.
                unsafe {
                    if want {
                        if !category_set {
                            if let Some(playback) = AVAudioSessionCategoryPlayback {
                                category_set = session
                                    .setCategory_withOptions_error(playback, AVAudioSessionCategoryOptions::DuckOthers)
                                    .is_ok();
                            }
                        }
                        is_active = session.setActive_error(true).is_ok();
                    } else {
                        // Whether or not it succeeds, stop claiming the
                        // session is up; the next prompt tries again.
                        let _ = session
                            .setActive_withOptions_error(false, AVAudioSessionSetActiveOptions::NotifyOthersOnDeactivation);
                        is_active = false;
                    }
                }
            }
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod imp {
    /// No voices here.
    #[must_use]
    pub fn voices() -> Vec<crate::nav::host::VoiceInfo> {
        Vec::new()
    }

    /// No speech on this platform yet: every call is a no-op.
    #[derive(Default)]
    pub struct Speaker;

    impl Speaker {
        #[must_use]
        pub fn new() -> Self {
            Self
        }

        #[must_use]
        pub fn voice_name(&self) -> Option<String> {
            None
        }

        pub fn use_voice(&mut self, _lang: &str, _name: Option<&str>) -> Option<String> {
            None
        }

        pub fn is_busy(&mut self) -> bool {
            false
        }

        pub fn speak(&mut self, _text: &str) {}

        pub fn speak_marked_up(&mut self, _text: &str, _ssml: Option<&str>) {}

        pub fn say_next(&mut self, _text: &str, _ssml: Option<&str>) {}

        pub fn stop(&mut self) {}

        pub fn poll(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::{speak_queues, Speaker};

    #[test]
    fn a_prompt_waits_behind_the_one_playing_but_never_two_deep() {
        assert!(!speak_queues(false, false), "nothing playing: say it now");
        assert!(speak_queues(true, false), "the opening prompt is not cut off by the next one");
        assert!(!speak_queues(true, true), "one already waits: interrupt both, say the newest");
        assert!(!speak_queues(false, true));
    }

    /// Speaks for real (inaudibly checked -- nobody listens in CI): proves
    /// the classes resolve, the voice lookup and `speakUtterance:` do not
    /// throw, and `speak` returns without blocking. `--ignored` only, since
    /// it makes the machine talk.
    #[test]
    #[ignore = "makes the machine talk; run by hand"]
    fn a_speaker_says_test_and_returns() {
        let mut speaker = Speaker::new();
        println!("[voice] picked: {}", speaker.voice_name().as_deref().unwrap_or("(system default)"));
        let started = std::time::Instant::now();
        speaker.speak("Test");
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "speak must not block");
        speaker.speak("Test, interrupted");
        std::thread::sleep(std::time::Duration::from_secs(2));
        speaker.poll();
        speaker.speak("Stopped before the end of this sentence");
        speaker.stop();
        speaker.poll();
        // What `nav::instructions::spoken_sentence` renders for an Early prompt.
        speaker.speak_marked_up(
            "In 200 metres, turn left onto Middle Road.",
            Some("<speak><prosody rate=\"95%\">In 200 metres,<break time=\"150ms\"/> turn left onto Middle Road.</prosody></speak>"),
        );
        std::thread::sleep(std::time::Duration::from_secs(4));
        speaker.poll();
    }
}
