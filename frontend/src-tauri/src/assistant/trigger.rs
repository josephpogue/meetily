// assistant/trigger.rs
//
// The cheap local prefilter that decides when the assistant volunteers an
// answer. It proposes; the fast lane disposes, by answering or skipping.
// Nothing here calls a model, so a wrong guess costs nothing but a spawn.
//
// Port of v1's TriggerEngine.swift. It works in whole utterances, never in
// the fragments the recognizer emits: a fragment reaching the lanes is not a
// wrong answer, it is no answer, since both lanes would skip it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::transcript::Speaker;

const MIN_WORDS: usize = 2;
const RECENT_FIRE_MEMORY: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    Manual,
    Gated,
    Continuous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    NameCalled,
    Question,
}

struct GapState {
    pending_question: Option<String>,
    fired_current_utterance: bool,
    recent_fires: Vec<String>,
}

pub struct TriggerEngine {
    /// Shared (not boxed) because a scheduled gap fire runs on its own tokio
    /// task, independent of whatever borrows `&mut self` next.
    pub on_fire: Arc<dyn Fn(String, TriggerReason) + Send + Sync>,
    mode: TriggerMode,
    quiet_gap: f64,
    listening: bool,
    names: Vec<String>,
    /// Joseph's name landed in the utterance currently being spoken on Them.
    named_in_current_utterance: bool,
    state: Arc<Mutex<GapState>>,
    gap_task: Option<tokio::task::JoinHandle<()>>,
}

impl TriggerEngine {
    pub fn new(on_fire: impl Fn(String, TriggerReason) + Send + Sync + 'static) -> Self {
        Self {
            on_fire: Arc::new(on_fire),
            mode: TriggerMode::Gated,
            quiet_gap: 2.0,
            listening: true,
            names: Vec::new(),
            named_in_current_utterance: false,
            state: Arc::new(Mutex::new(GapState {
                pending_question: None,
                fired_current_utterance: false,
                recent_fires: Vec::new(),
            })),
            gap_task: None,
        }
    }

    pub fn update(&mut self, mode: TriggerMode, quiet_gap: f64, listening: bool, names: Vec<String>) {
        self.mode = mode;
        self.quiet_gap = quiet_gap;
        self.listening = listening;
        self.names = names;
        if mode == TriggerMode::Manual || !listening {
            self.cancel_pending();
        }
    }

    pub fn reset(&mut self) {
        self.cancel_pending();
        self.named_in_current_utterance = false;
        let mut state = self.state.lock().unwrap();
        state.fired_current_utterance = false;
        state.recent_fires.clear();
    }

    /// The running buffer for Them, after each finalized fragment. This exists
    /// for one case: a name-call, where waiting costs the whole point of the
    /// feature. Even here it never fires on a half-sentence; the name arms the
    /// utterance and fires early only when the buffer already reads as a
    /// finished question.
    pub fn consume_running(&mut self, speaker: Speaker, text: &str) {
        if !self.listening || self.mode == TriggerMode::Manual || speaker != Speaker::Them {
            return;
        }
        if self.state.lock().unwrap().fired_current_utterance {
            return;
        }

        self.restart_gap_if_pending();

        if !mentions_names(text, &self.names) {
            return;
        }
        self.named_in_current_utterance = true;
        if !looks_complete(text) || !is_substantive(text) || self.is_repeat(text) {
            return;
        }
        self.fire(text.to_string(), TriggerReason::NameCalled);
    }

    /// Them went quiet and its utterance is complete. Question triggers are
    /// decided here, with the whole utterance handed on.
    pub fn consume_utterance(&mut self, speaker: Speaker, text: &str) {
        self.consume_utterance_inner(speaker, text);
        if speaker == Speaker::Them {
            self.named_in_current_utterance = false;
            self.state.lock().unwrap().fired_current_utterance = false;
        }
    }

    fn consume_utterance_inner(&mut self, speaker: Speaker, text: &str) {
        if !self.listening || self.mode == TriggerMode::Manual {
            return;
        }
        self.restart_gap_if_pending();

        let already_fired = self.state.lock().unwrap().fired_current_utterance;
        if speaker != Speaker::Them || already_fired || !is_substantive(text) || self.is_repeat(text) {
            return;
        }

        // A name anywhere in the utterance fires at once, gap or no gap.
        if self.named_in_current_utterance || mentions_names(text, &self.names) {
            self.fire(text.to_string(), TriggerReason::NameCalled);
            return;
        }

        if !looks_like_question(text) {
            return;
        }

        match self.mode {
            TriggerMode::Continuous => self.fire(text.to_string(), TriggerReason::Question),
            TriggerMode::Gated => {
                self.state.lock().unwrap().pending_question = Some(text.to_string());
                self.schedule_gap();
            }
            TriggerMode::Manual => {}
        }
    }

    pub fn cancel_pending(&mut self) {
        if let Some(task) = self.gap_task.take() {
            task.abort();
        }
        self.state.lock().unwrap().pending_question = None;
    }

    fn fire(&mut self, text: String, reason: TriggerReason) {
        self.cancel_pending();
        {
            let mut state = self.state.lock().unwrap();
            state.fired_current_utterance = true;
            remember(&mut state.recent_fires, text.clone());
        }
        (self.on_fire)(text, reason);
    }

    /// True when this candidate is a question already answered. Not a prefix
    /// or containment test: the recognizer revises as it goes, so one spoken
    /// question arrives as several disagreeing strings. Compare what the
    /// sentences are about instead.
    fn is_repeat(&self, text: &str) -> bool {
        let a = content_words(text);
        if a.is_empty() {
            return true;
        }
        let state = self.state.lock().unwrap();
        state.recent_fires.iter().any(|earlier| {
            let b = content_words(earlier);
            if b.is_empty() {
                return false;
            }
            let overlap = a.intersection(&b).count();
            (overlap as f64) / (a.len().min(b.len()) as f64) >= 0.7
        })
    }

    /// Any speech is the room not being silent, so a waiting gap restarts.
    fn restart_gap_if_pending(&mut self) {
        let has_pending = self.state.lock().unwrap().pending_question.is_some();
        if has_pending {
            self.schedule_gap();
        }
    }

    fn schedule_gap(&mut self) {
        if let Some(task) = self.gap_task.take() {
            task.abort();
        }
        let remaining = Duration::from_secs_f64(self.quiet_gap.max(0.0));
        let state = self.state.clone();
        let on_fire = self.on_fire.clone();
        self.gap_task = Some(tokio::spawn(async move {
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            let question = {
                let mut s = state.lock().unwrap();
                match s.pending_question.take() {
                    Some(q) => {
                        s.fired_current_utterance = true;
                        remember(&mut s.recent_fires, q.clone());
                        Some(q)
                    }
                    None => None,
                }
            };
            if let Some(q) = question {
                on_fire(q, TriggerReason::Question);
            }
        }));
    }
}

fn remember(recent_fires: &mut Vec<String>, text: String) {
    recent_fires.push(text);
    if recent_fires.len() > RECENT_FIRE_MEMORY {
        recent_fires.remove(0);
    }
}

/// Enough words to be a question at all. Counts plain words, not the stemmed
/// content words the repeat guard uses: a real short question can be almost
/// all stopwords ("So what does that do to the Q4 migration plan?" has two
/// content words), and every noise utterance ("?", ".", "3.", "Q.") has none.
fn is_substantive(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .collect();
    words.len() >= MIN_WORDS
}

fn mentions_names(text: &str, names: &[String]) -> bool {
    let lower = text.to_lowercase();
    let words: HashSet<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    names.iter().any(|n| words.contains(n.to_lowercase().as_str()))
}

/// A question mark, or an interrogative opening. Both are needed: live
/// transcription drops terminal punctuation often enough that a mark-only
/// test would miss most spoken questions.
fn looks_like_question(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains('?') {
        return true;
    }

    let lower = t.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let Some(first) = words.first() else {
        return false;
    };

    if INTERROGATIVE_OPENERS.contains(first) {
        return true;
    }
    // "so what happens if...", "and how do we...": one filler then a wh-word.
    if FILLERS.contains(first) && words.len() > 1 && INTERROGATIVE_OPENERS.contains(&words[1]) {
        return true;
    }
    // An interrogative can also open a later clause: "Joseph, which of the four...".
    for i in 1..words.len().min(6) {
        if INTERROGATIVE_OPENERS.contains(&words[i]) && LEAD_INS.contains(&words[i - 1]) {
            return true;
        }
    }
    for phrase in ASK_PHRASES {
        if lower.contains(phrase) {
            return true;
        }
    }
    false
}

/// Whether a running buffer has closed on an actual question, the only case
/// where firing before the boundary is safe. A question mark and nothing
/// else: live drafts are full of stray mid-sentence periods, and accepting
/// those would fire on half a question.
fn looks_complete(text: &str) -> bool {
    text.trim().ends_with('?')
}

const INTERROGATIVE_OPENERS: &[&str] = &[
    "what", "why", "how", "when", "where", "who", "which", "whose", "do", "does", "did", "is",
    "are", "was", "were", "am", "can", "could", "should", "would", "will", "shall", "have",
    "has", "had", "may", "might",
];
const FILLERS: &[&str] = &["so", "and", "but", "ok", "okay", "now", "then", "well"];
/// Words that commonly sit right before the real question starts.
const LEAD_INS: &[&str] = &[
    "joseph", "joe", "so", "and", "but", "then", "now", "well", "okay", "ok",
];
const ASK_PHRASES: &[&str] = &[
    "thoughts on",
    "your take",
    "any idea",
    "any thoughts",
    "what do you think",
    "let me know",
    "walk us through",
    "walk me through",
];

// TODO(task-6): CardFormat.swift owns contentWords/stem/stopwords in v1; when
// card.rs lands, move this there and have trigger.rs import it, matching v1's
// dependency direction (Trigger depends on Card, not the reverse).
const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "your", "yours", "they", "them", "then", "than", "have",
    "does", "will", "would", "could", "should", "which", "what", "when", "where", "into", "onto",
    "over", "under", "about", "actually", "really", "just", "only", "also", "both", "some",
    "more", "most", "much", "very", "here", "there", "these", "those", "still", "been", "being",
    "were", "weren", "wasn", "isn", "aren", "didn", "doesn",
];

fn content_words(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !STOPWORDS.contains(w))
        .map(stem)
        .collect()
}

/// Crude plural and third-person stripping. Load-bearing rather than
/// cosmetic: without it "Proposal 3 holds" and "Proposals 3 and 4 hold" share
/// no words at all. Words ending in a double s are left alone.
fn stem(word: &str) -> String {
    let char_count = word.chars().count();
    if char_count >= 5 && word.ends_with('s') && !word.ends_with("ss") {
        let mut chars: Vec<char> = word.chars().collect();
        chars.pop();
        chars.into_iter().collect()
    } else {
        word.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_capture() -> (TriggerEngine, Arc<Mutex<Vec<(String, TriggerReason)>>>) {
        let fires = Arc::new(Mutex::new(Vec::new()));
        let captured = fires.clone();
        let engine = TriggerEngine::new(move |text, reason| {
            captured.lock().unwrap().push((text, reason));
        });
        (engine, fires)
    }

    #[test]
    fn noise_marks_never_fire() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        for noise in ["?", ".", "3.", "Q."] {
            engine.consume_utterance(Speaker::Them, noise);
        }
        assert!(fires.lock().unwrap().is_empty());
    }

    #[test]
    fn short_question_all_stopwords_fires() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_utterance(Speaker::Them, "So what does that do to the Q4 migration plan?");
        let fired = fires.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1, TriggerReason::Question);
    }

    #[test]
    fn revised_drafts_of_one_question_fire_once() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_utterance(Speaker::Them, "does that actually holds up order proposal which is Q");
        engine.consume_utterance(Speaker::Them, "does that actually holds up by Q3 when");
        engine.consume_utterance(Speaker::Them, "does that actually holds up");
        assert_eq!(fires.lock().unwrap().len(), 1);
    }

    #[test]
    fn name_fires_even_without_question_shape() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(
            TriggerMode::Continuous,
            2.0,
            true,
            vec!["joseph".to_string(), "joe".to_string()],
        );
        engine.consume_utterance(Speaker::Them, "Joseph, walk us through the rollout");
        let fired = fires.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1, TriggerReason::NameCalled);
    }

    #[test]
    fn you_channel_never_triggers() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_utterance(Speaker::You, "What does that do to the migration plan?");
        assert!(fires.lock().unwrap().is_empty());
    }

    #[test]
    fn manual_mode_never_volunteers() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Manual, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_utterance(Speaker::Them, "Joseph, what does that do to the migration plan?");
        assert!(fires.lock().unwrap().is_empty());
    }

    #[test]
    fn name_arms_and_fires_early_on_complete_question_mid_utterance() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_running(Speaker::Them, "Joseph, what does the rollout look like?");
        let fired = fires.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1, TriggerReason::NameCalled);
    }

    #[test]
    fn name_arms_but_does_not_fire_on_incomplete_running_text() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Continuous, 2.0, true, vec!["joseph".to_string()]);
        engine.consume_running(Speaker::Them, "Joseph, if order volume");
        assert!(fires.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gated_question_fires_after_quiet_gap() {
        let (mut engine, fires) = engine_with_capture();
        engine.update(TriggerMode::Gated, 0.05, true, vec!["joseph".to_string()]);
        engine.consume_utterance(Speaker::Them, "What does that do to the migration plan?");
        assert!(
            fires.lock().unwrap().is_empty(),
            "should not fire before the gap elapses"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let fired = fires.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].1, TriggerReason::Question);
    }
}
