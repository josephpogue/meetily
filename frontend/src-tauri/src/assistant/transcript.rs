// assistant/transcript.rs
//
// Ordered, labeled transcript log built from TranscriptUpdate, plus the
// utterance assembler: consecutive same-speaker segments merge into one
// utterance until a quiet gap or a speaker flip closes it.

use crate::audio::TranscriptUpdate;

/// Segments this close together (by audio_start_time vs. the open
/// utterance's end) keep merging into the same utterance.
const UTTERANCE_GAP_SECS: f64 = 1.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Them,
    Unknown,
}

impl Speaker {
    fn from_source(source: &str) -> Self {
        match source {
            "mic" => Speaker::You,
            "system" => Speaker::Them,
            _ => Speaker::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Speaker::You => "You",
            Speaker::Them => "Them",
            Speaker::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssemblerOut {
    /// Partial preview, or a still-open utterance that just grew.
    Running { speaker: Speaker, text: String },
    /// Closed: quiet gap >= 1.2 s or a speaker flip.
    Utterance { speaker: Speaker, text: String },
}

struct OpenUtterance {
    speaker: Speaker,
    /// Text merged from finalized segments only; a partial never lands here.
    committed_text: String,
    /// Latest partial preview for the segment still in flight, replaced
    /// wholesale (never appended) when its final arrives.
    pending_partial: Option<String>,
    end_time: f64,
}

#[derive(Default)]
pub struct TranscriptLog {
    finals: Vec<(Speaker, String, f64)>,
    open: Option<OpenUtterance>,
}

impl TranscriptLog {
    pub fn ingest(&mut self, u: &TranscriptUpdate) -> Vec<AssemblerOut> {
        let speaker = Speaker::from_source(&u.source);
        let mut out = Vec::new();

        let continues_open = match &self.open {
            Some(open) => {
                open.speaker == speaker
                    && (u.audio_start_time - open.end_time) < UTTERANCE_GAP_SECS
            }
            None => false,
        };

        if !continues_open {
            if let Some(closed) = self.close_open() {
                out.push(closed);
            }
            self.open = Some(OpenUtterance {
                speaker,
                committed_text: String::new(),
                pending_partial: None,
                end_time: u.audio_end_time,
            });
        }

        let open = self.open.as_mut().expect("just ensured an open utterance");
        if u.is_partial {
            open.pending_partial = Some(u.text.clone());
        } else {
            if !open.committed_text.is_empty() {
                open.committed_text.push(' ');
            }
            open.committed_text.push_str(&u.text);
            open.pending_partial = None;
        }
        open.end_time = u.audio_end_time;

        out.push(AssemblerOut::Running {
            speaker,
            text: self.preview_text(),
        });
        out
    }

    fn preview_text(&self) -> String {
        let Some(open) = &self.open else {
            return String::new();
        };
        match &open.pending_partial {
            Some(partial) if !open.committed_text.is_empty() => {
                format!("{} {}", open.committed_text, partial)
            }
            Some(partial) => partial.clone(),
            None => open.committed_text.clone(),
        }
    }

    /// Commits the open utterance's finalized text to the log, if any, and
    /// returns the Utterance event that reports it as closed. An utterance
    /// that never finalized (partial-only) closes silently.
    fn close_open(&mut self) -> Option<AssemblerOut> {
        let open = self.open.take()?;
        if open.committed_text.is_empty() {
            return None;
        }
        self.finals
            .push((open.speaker, open.committed_text.clone(), open.end_time));
        Some(AssemblerOut::Utterance {
            speaker: open.speaker,
            text: open.committed_text,
        })
    }

    /// New finals since `cursor`, labeled "You:"/"Them:", and the cursor to pass next time.
    pub fn delta_since(&self, cursor: usize) -> (String, usize) {
        let start = cursor.min(self.finals.len());
        let text = self.finals[start..]
            .iter()
            .map(|(speaker, text, _)| format!("{}: {}", speaker.label(), text))
            .collect::<Vec<_>>()
            .join("\n");
        (text, self.finals.len())
    }

    /// Labeled finals from the last `seconds` of audio, optionally filtered to one speaker.
    pub fn window(&self, seconds: f64, speaker: Option<Speaker>) -> String {
        let now = self
            .finals
            .iter()
            .map(|(_, _, end)| *end)
            .chain(self.open.as_ref().map(|o| o.end_time))
            .fold(0.0_f64, f64::max);

        self.finals
            .iter()
            .filter(|(_, _, end)| now - end <= seconds)
            .filter(|(s, _, _)| speaker.map(|want| want == *s).unwrap_or(true))
            .map(|(s, text, _)| format!("{}: {}", s.label(), text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn all(&self) -> String {
        self.finals
            .iter()
            .map(|(speaker, text, _)| format!("{}: {}", speaker.label(), text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tu(source: &str, text: &str, start: f64, end: f64, is_partial: bool, seq: u64) -> TranscriptUpdate {
        TranscriptUpdate {
            text: text.to_string(),
            timestamp: "00:00:00".to_string(),
            source: source.to_string(),
            sequence_id: seq,
            chunk_start_time: start,
            is_partial,
            confidence: 1.0,
            audio_start_time: start,
            audio_end_time: end,
            duration: end - start,
        }
    }

    #[test]
    fn merges_consecutive_same_speaker_within_gap() {
        let mut log = TranscriptLog::default();
        log.ingest(&tu("system", "Hi", 0.0, 1.0, false, 1));
        let out = log.ingest(&tu("system", "there", 1.5, 2.0, false, 2));
        assert_eq!(
            out,
            vec![AssemblerOut::Running {
                speaker: Speaker::Them,
                text: "Hi there".to_string()
            }]
        );
    }

    #[test]
    fn gap_closes_same_speaker_utterance() {
        let mut log = TranscriptLog::default();
        log.ingest(&tu("system", "First", 0.0, 1.0, false, 1));
        let out = log.ingest(&tu("system", "Second", 3.0, 4.0, false, 2));
        assert_eq!(
            out,
            vec![
                AssemblerOut::Utterance {
                    speaker: Speaker::Them,
                    text: "First".to_string()
                },
                AssemblerOut::Running {
                    speaker: Speaker::Them,
                    text: "Second".to_string()
                },
            ]
        );
    }

    #[test]
    fn speaker_flip_closes_open_utterance() {
        let mut log = TranscriptLog::default();
        log.ingest(&tu("system", "Hi there", 0.0, 1.0, false, 1));
        let out = log.ingest(&tu("mic", "yes", 1.2, 2.0, false, 2));
        assert_eq!(
            out,
            vec![
                AssemblerOut::Utterance {
                    speaker: Speaker::Them,
                    text: "Hi there".to_string()
                },
                AssemblerOut::Running {
                    speaker: Speaker::You,
                    text: "yes".to_string()
                },
            ]
        );
    }

    #[test]
    fn partials_replaced_not_appended_by_final() {
        let mut log = TranscriptLog::default();
        let r1 = log.ingest(&tu("system", "Hel", 0.0, 0.3, true, 5));
        assert_eq!(
            r1,
            vec![AssemblerOut::Running {
                speaker: Speaker::Them,
                text: "Hel".to_string()
            }]
        );
        let r2 = log.ingest(&tu("system", "Hello wor", 0.0, 0.6, true, 5));
        assert_eq!(
            r2,
            vec![AssemblerOut::Running {
                speaker: Speaker::Them,
                text: "Hello wor".to_string()
            }]
        );
        let r3 = log.ingest(&tu("system", "Hello world", 0.0, 1.0, false, 5));
        assert_eq!(
            r3,
            vec![AssemblerOut::Running {
                speaker: Speaker::Them,
                text: "Hello world".to_string()
            }]
        );
    }

    #[test]
    fn delta_since_returns_new_labeled_finals() {
        let mut log = TranscriptLog::default();
        log.ingest(&tu("mic", "hello", 0.0, 1.0, false, 1));
        log.ingest(&tu("system", "hi there", 3.0, 4.0, false, 2));
        let (text, cursor) = log.delta_since(0);
        assert_eq!(text, "You: hello");
        assert_eq!(cursor, 1);

        log.ingest(&tu("mic", "ok", 6.0, 6.5, false, 3));
        let (text2, cursor2) = log.delta_since(cursor);
        assert_eq!(text2, "Them: hi there");
        assert_eq!(cursor2, 2);
    }

    #[test]
    fn window_filters_by_speaker_and_recency() {
        let mut log = TranscriptLog::default();
        log.ingest(&tu("system", "old them", 0.0, 1.0, false, 1));
        log.ingest(&tu("mic", "you", 1.2, 1.5, false, 2));
        log.ingest(&tu("system", "recent them", 20.0, 21.0, false, 3));
        log.ingest(&tu("mic", "closer", 22.0, 22.5, false, 4));

        let out = log.window(15.0, Some(Speaker::Them));
        assert_eq!(out, "Them: recent them");
    }
}
