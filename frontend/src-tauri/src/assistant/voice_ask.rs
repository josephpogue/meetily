// assistant/voice_ask.rs
//
// Push-to-talk voice ask: Joseph speaks a question to the assistant instead
// of typing it.
//
// Nothing new listens. His microphone is already captured and already
// running through transcription as the "You" channel, because the
// transcript needs it. A voice ask is just that same text, bracketed
// between a start and a stop. No new permission, no new process, no audio
// leaving the machine.
//
// Why it reads the live (draft) text and not the finalized text: finals
// arrive up to ~20 s late and in mid-word fragments (see transcript.rs's
// header). Waiting for them would mean speaking a question and watching
// nothing happen for most of a minute. Drafts are ~1-1.5 s behind the
// voice, which is what a person expects from something they are talking to.
//
// Port of v1's VoiceAsk.swift. v1's timers run as cancellable Swift Tasks
// on the main actor, so `finish()` called from inside a timer's own
// continuation still has full access to `self`. A spawned tokio task has no
// such access back to `&mut self`, so every timer here reaches shared state
// through an `Arc<Mutex<CaptureState>>` instead, and the public methods are
// thin wrappers over free functions that take that same `Arc`.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq)]
pub enum VoiceState {
    /// Not capturing. The ask box behaves as a normal text field.
    Off,
    /// Capturing. `heard` is the running text so far, shown live in the ask box.
    Listening { heard: String },
    /// Capture closed, question handed to the lanes; shown briefly, then `Off`.
    Submitting { question: String },
}

struct CaptureState {
    is_capturing: bool,
    /// Utterances that closed during this capture, in order.
    settled: Vec<String>,
    /// The utterance still being spoken, which transcription keeps revising.
    running: String,
    silence_task: Option<JoinHandle<()>>,
    cap_task: Option<JoinHandle<()>>,
    linger_task: Option<JoinHandle<()>>,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self {
            is_capturing: false,
            settled: Vec::new(),
            running: String::new(),
            silence_task: None,
            cap_task: None,
            linger_task: None,
        }
    }
}

/// Everything heard so far, spoken order.
fn heard(s: &CaptureState) -> String {
    let mut parts: Vec<&str> = s.settled.iter().map(|s| s.as_str()).collect();
    if !s.running.is_empty() {
        parts.push(&s.running);
    }
    parts.join(" ").trim().to_string()
}

fn stop_timers(s: &mut CaptureState) {
    if let Some(h) = s.silence_task.take() {
        h.abort();
    }
    if let Some(h) = s.cap_task.take() {
        h.abort();
    }
}

pub struct VoiceAsk {
    /// The finished question. Fires once per capture, never with empty text.
    pub on_submit: Arc<dyn Fn(String) + Send + Sync>,
    /// Mirrors capture state so the rail can show what it heard.
    pub publish: Arc<dyn Fn(VoiceState) + Send + Sync>,
    /// Quiet time after he stops talking before the question is submitted
    /// on its own. This is on top of the utterance assembler's own 1.2 s
    /// boundary, so the real pause he feels is roughly this plus that.
    silence_gap: Duration,
    /// Nobody asks a single question for this long; a stuck capture ends
    /// itself rather than swallowing the rest of the meeting.
    max_capture: Duration,
    /// How long the submitted question stays on screen before the box clears.
    submit_linger: Duration,
    state: Arc<Mutex<CaptureState>>,
}

impl VoiceAsk {
    pub fn new(
        on_submit: impl Fn(String) + Send + Sync + 'static,
        publish: impl Fn(VoiceState) + Send + Sync + 'static,
    ) -> Self {
        Self::with_timing(
            on_submit,
            publish,
            Duration::from_secs_f64(1.5),
            Duration::from_secs_f64(45.0),
            Duration::from_secs_f64(1.4),
        )
    }

    pub fn with_timing(
        on_submit: impl Fn(String) + Send + Sync + 'static,
        publish: impl Fn(VoiceState) + Send + Sync + 'static,
        silence_gap: Duration,
        max_capture: Duration,
        submit_linger: Duration,
    ) -> Self {
        Self {
            on_submit: Arc::new(on_submit),
            publish: Arc::new(publish),
            silence_gap,
            max_capture,
            submit_linger,
            state: Arc::new(Mutex::new(CaptureState::default())),
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.state.lock().unwrap().is_capturing
    }

    /// Idempotent while already capturing.
    pub fn start(&mut self) {
        do_start(
            &self.state,
            &self.publish,
            &self.on_submit,
            self.max_capture,
            self.submit_linger,
        );
    }

    /// Submits what was heard, or publishes `Off` silently if nothing was.
    pub fn finish(&mut self) {
        do_finish(&self.state, &self.publish, &self.on_submit, self.submit_linger);
    }

    /// Throws the capture away and asks nothing.
    pub fn cancel(&mut self) {
        do_cancel(&self.state, &self.publish);
    }

    /// The live draft region grew or was revised. Drafts replace rather
    /// than append: the recognizer has already assembled the whole
    /// in-flight region, so concatenating successive drafts would repeat
    /// every word many times.
    pub fn note_running(&mut self, text: &str) {
        do_note_running(
            &self.state,
            &self.publish,
            &self.on_submit,
            self.silence_gap,
            self.submit_linger,
            text,
        );
    }

    /// The channel went quiet and that utterance is final for live
    /// purposes. It moves into the settled list so the next one does not
    /// overwrite it: a question spoken as two sentences has to survive as
    /// two sentences.
    pub fn note_utterance(&mut self, text: &str) {
        do_note_utterance(
            &self.state,
            &self.publish,
            &self.on_submit,
            self.silence_gap,
            self.submit_linger,
            text,
        );
    }
}

fn do_start(
    state: &Arc<Mutex<CaptureState>>,
    publish: &Arc<dyn Fn(VoiceState) + Send + Sync>,
    on_submit: &Arc<dyn Fn(String) + Send + Sync>,
    max_capture: Duration,
    submit_linger: Duration,
) {
    {
        let mut s = state.lock().unwrap();
        if s.is_capturing {
            return;
        }
        if let Some(h) = s.linger_task.take() {
            h.abort();
        }
        s.is_capturing = true;
        s.settled.clear();
        s.running.clear();
    }
    publish(VoiceState::Listening { heard: String::new() });
    log::debug!("voice ask: listening");

    let cap_state = state.clone();
    let cap_publish = publish.clone();
    let cap_on_submit = on_submit.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(max_capture).await;
        log::debug!("voice ask: hit the capture cap, submitting what it has");
        do_finish(&cap_state, &cap_publish, &cap_on_submit, submit_linger);
    });
    let mut s = state.lock().unwrap();
    if let Some(old) = s.cap_task.replace(handle) {
        old.abort();
    }
}

fn do_finish(
    state: &Arc<Mutex<CaptureState>>,
    publish: &Arc<dyn Fn(VoiceState) + Send + Sync>,
    on_submit: &Arc<dyn Fn(String) + Send + Sync>,
    submit_linger: Duration,
) {
    let question = {
        let mut s = state.lock().unwrap();
        if !s.is_capturing {
            return;
        }
        let question = heard(&s);
        stop_timers(&mut s);
        s.is_capturing = false;
        question
    };

    if question.is_empty() {
        log::debug!("voice ask: nothing heard, cancelled");
        publish(VoiceState::Off);
        return;
    }

    log::debug!("voice ask: asking: {}", question);
    publish(VoiceState::Submitting {
        question: question.clone(),
    });
    on_submit(question);

    // Leave it on screen long enough to read what it thought he said, then
    // hand the box back to the keyboard.
    let linger_publish = publish.clone();
    let linger_state = state.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(submit_linger).await;
        linger_publish(VoiceState::Off);
        linger_state.lock().unwrap().linger_task = None;
    });
    let mut s = state.lock().unwrap();
    if let Some(old) = s.linger_task.replace(handle) {
        old.abort();
    }
}

fn do_cancel(state: &Arc<Mutex<CaptureState>>, publish: &Arc<dyn Fn(VoiceState) + Send + Sync>) {
    let should_publish = {
        let mut s = state.lock().unwrap();
        if !s.is_capturing && s.linger_task.is_none() {
            return;
        }
        stop_timers(&mut s);
        if let Some(h) = s.linger_task.take() {
            h.abort();
        }
        s.is_capturing = false;
        s.settled.clear();
        s.running.clear();
        true
    };
    if should_publish {
        log::debug!("voice ask: cancelled");
        publish(VoiceState::Off);
    }
}

/// Restarted on every scrap of speech, so the clock only runs while he is
/// actually quiet.
fn arm_silence(
    state: &Arc<Mutex<CaptureState>>,
    publish: &Arc<dyn Fn(VoiceState) + Send + Sync>,
    on_submit: &Arc<dyn Fn(String) + Send + Sync>,
    silence_gap: Duration,
    submit_linger: Duration,
) {
    let silence_state = state.clone();
    let silence_publish = publish.clone();
    let silence_on_submit = on_submit.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(silence_gap).await;
        do_finish(&silence_state, &silence_publish, &silence_on_submit, submit_linger);
    });
    let mut s = state.lock().unwrap();
    if let Some(old) = s.silence_task.replace(handle) {
        old.abort();
    }
}

fn do_note_running(
    state: &Arc<Mutex<CaptureState>>,
    publish: &Arc<dyn Fn(VoiceState) + Send + Sync>,
    on_submit: &Arc<dyn Fn(String) + Send + Sync>,
    silence_gap: Duration,
    submit_linger: Duration,
    text: &str,
) {
    let clean = text.trim();
    if clean.is_empty() {
        return;
    }
    let current = {
        let mut s = state.lock().unwrap();
        if !s.is_capturing {
            return;
        }
        s.running = clean.to_string();
        heard(&s)
    };
    publish(VoiceState::Listening { heard: current });
    arm_silence(state, publish, on_submit, silence_gap, submit_linger);
}

fn do_note_utterance(
    state: &Arc<Mutex<CaptureState>>,
    publish: &Arc<dyn Fn(VoiceState) + Send + Sync>,
    on_submit: &Arc<dyn Fn(String) + Send + Sync>,
    silence_gap: Duration,
    submit_linger: Duration,
    text: &str,
) {
    let clean = text.trim();
    let current = {
        let mut s = state.lock().unwrap();
        if !s.is_capturing {
            return;
        }
        if clean.is_empty() {
            s.running.clear();
            return;
        }
        s.settled.push(clean.to_string());
        s.running.clear();
        heard(&s)
    };
    publish(VoiceState::Listening { heard: current });
    arm_silence(state, publish, on_submit, silence_gap, submit_linger);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder() -> (Arc<Mutex<Vec<VoiceState>>>, Arc<Mutex<Vec<String>>>) {
        (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())))
    }

    fn make(
        published: Arc<Mutex<Vec<VoiceState>>>,
        submitted: Arc<Mutex<Vec<String>>>,
    ) -> VoiceAsk {
        VoiceAsk::new(
            move |q| submitted.lock().unwrap().push(q),
            move |s| published.lock().unwrap().push(s),
        )
    }

    #[tokio::test]
    async fn two_settled_and_one_running_join_in_spoken_order() {
        let (published, submitted) = recorder();
        let mut voice = make(published.clone(), submitted);

        voice.start();
        voice.note_utterance("Hello there,");
        voice.note_utterance("quick question.");
        voice.note_running("What did I");

        assert_eq!(voice.test_heard(), "Hello there, quick question. What did I");
        let last = published.lock().unwrap().last().cloned();
        assert_eq!(
            last,
            Some(VoiceState::Listening {
                heard: "Hello there, quick question. What did I".to_string()
            })
        );
    }

    #[tokio::test]
    async fn finish_with_nothing_heard_publishes_off_and_never_submits() {
        let (published, submitted) = recorder();
        let mut voice = make(published.clone(), submitted.clone());

        voice.start();
        voice.finish();

        assert!(submitted.lock().unwrap().is_empty());
        assert_eq!(published.lock().unwrap().last(), Some(&VoiceState::Off));
        assert!(!voice.is_capturing());
    }

    #[tokio::test]
    async fn second_start_during_capture_is_a_no_op() {
        let (published, submitted) = recorder();
        let mut voice = make(published.clone(), submitted);

        voice.start();
        voice.note_utterance("keep this");
        let calls_before = published.lock().unwrap().len();

        voice.start();

        assert_eq!(voice.test_heard(), "keep this", "a second start must not clear what was heard");
        assert_eq!(
            published.lock().unwrap().len(),
            calls_before,
            "a no-op start must not publish anything"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silence_gap_auto_submits_after_the_quiet_period() {
        let (published, submitted) = recorder();
        let mut voice = make(published.clone(), submitted.clone());

        voice.start();
        voice.note_utterance("what did I miss");

        tokio::time::sleep(Duration::from_millis(1600)).await;

        assert_eq!(submitted.lock().unwrap().as_slice(), ["what did I miss".to_string()]);
        assert!(!voice.is_capturing());
    }

    impl VoiceAsk {
        fn test_heard(&self) -> String {
            heard(&self.state.lock().unwrap())
        }
    }
}
