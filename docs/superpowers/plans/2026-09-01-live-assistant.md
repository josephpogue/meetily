# Meetily Live Assistant Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a live in-meeting Claude assistant (typed and spoken asks, proactive trigger cards, explain/catch-up, end-of-meeting note) to the meetily fork, driven by subscription `claude -p` spawns.

**Architecture:** New isolated Rust module `assistant/` in the Tauri core (claude CLI runner, answer lanes, trigger engine, voice ask, note drafter) fed by the existing `transcript-update` event; new `AssistantPanel` React component beside the transcript; one upstream edit tags transcript segments mic/system before the audio mixer erases identity.

**Tech Stack:** Rust (tokio, tauri 2, sqlx), Next.js 14 + TypeScript + Tailwind + shadcn, `claude` CLI 2.x on subscription auth.

**Spec:** `docs/superpowers/plans/../specs/2026-09-01-live-assistant-design.md` — read it first; every contract there is binding.

**Reference implementation:** meeting-assistant v1 at `/Users/joseph.pogue/brain/departments/personal/projects/meeting-assistant/` (read-only). Port targets:
- `app/Sources/MeetingAssistant/Engine/ClaudeCLI.swift` → Task 3
- `app/Sources/MeetingAssistant/Engine/TriggerEngine.swift` → Task 5
- `app/Sources/MeetingAssistant/Engine/CardFormat.swift` → Task 6
- `app/Sources/MeetingAssistant/Engine/AnswerLanes.swift` → Task 7
- `app/Sources/MeetingAssistant/Engine/VoiceAsk.swift` → Task 8
- `prompts/fast.md`, `prompts/deep.md`, `prompts/note.md` → Task 7/9

## Global Constraints

- Subscription only: model calls happen exclusively through local `claude -p` spawns. Never `api.anthropic.com`, never API keys, never touch the existing summary providers.
- Nothing is written outside the app without an explicit Save (vault note only; `ASSISTANT_DRY_RUN=1` must disable it in tests).
- The recording/transcription path must never block on or fail because of the assistant. Every assistant entry point catches its own errors.
- Feature/module/table/event naming: `assistant` (the names "claude"/"anthropic" are taken by the existing summary provider).
- No em dashes in any user-visible copy, prompt text, or commit message. Commits: concise, high-level, no AI co-author lines.
- Upstream files may be edited only where a task names them. Everything else goes in new files.
- Rust work dir: `frontend/src-tauri/`. Run `source "$HOME/.cargo/env"` first in every shell. Verify Rust with `cargo check -p app 2>&1 | tail -20` (package name: check `frontend/src-tauri/Cargo.toml [package] name` first; use that). Frontend typecheck: `cd frontend && npx tsc --noEmit`.
- The first `cargo check` compiles whisper-rs and takes many minutes. Start it once, early, in the background; later checks are incremental.
- Tooling setup (once, before Task 1): `corepack enable && corepack prepare pnpm@9 --activate`, then `cd frontend && pnpm install`.

## Execution shape

Two parallel lanes after Task 2 lands, then QA:
- **Backend lane** (Tasks 1–10, sequential within the lane): everything under `frontend/src-tauri/`.
- **Frontend lane** (Tasks 11–13, sequential within the lane): everything under `frontend/src/`. Builds against the pinned contract in the spec ("Commands and events"), not against the Rust code, so it does not wait for the backend lane.
- Task 14 (hotkeys) and Task 15 (E2E) run after both lanes merge their work.

The two lanes share one worktree and one branch but disjoint file sets. Each lane commits only its own files. Before any commit: `git status` and confirm every staged path belongs to your lane.

---

### Task 1: Mic/system source tagging (backend lane)

**Files:**
- Modify: `frontend/src-tauri/src/audio/pipeline.rs` (mix + re-wrap sites near lines 766–898, 914–919, 1036–1061)
- Modify: `frontend/src-tauri/src/audio/transcription/worker.rs:211` (source assignment)
- Test: inline `#[cfg(test)]` in `pipeline.rs`

**Interfaces:**
- Produces: `TranscriptUpdate.source` is now `"mic"`, `"system"`, or `"mixed"` (was always `"Audio"`).

- [ ] **Step 1: Write the failing test** for the tag function in `pipeline.rs`:

```rust
#[cfg(test)]
mod source_tag_tests {
    use super::*;
    #[test]
    fn tags_dominant_channel() {
        let loud: Vec<f32> = (0..1600).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        let quiet: Vec<f32> = vec![0.01; 1600];
        assert_eq!(dominant_source(&loud, &quiet), SegmentSource::Mic);
        assert_eq!(dominant_source(&quiet, &loud), SegmentSource::System);
        assert_eq!(dominant_source(&loud, &loud), SegmentSource::Mixed);
    }
}
```

- [ ] **Step 2: Run it** (`cargo test -p <pkg> dominant_source -- --nocapture` from `frontend/src-tauri/`). Expected: compile FAIL, `dominant_source` not found.
- [ ] **Step 3: Implement.** In `pipeline.rs`: add

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SegmentSource { Mic, System, Mixed }

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() { return 0.0; }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Mic wins or system wins only when clearly louder (2x RMS); else Mixed.
fn dominant_source(mic: &[f32], sys: &[f32]) -> SegmentSource {
    let (m, s) = (rms(mic), rms(sys));
    if m > s * 2.0 { SegmentSource::Mic }
    else if s > m * 2.0 { SegmentSource::System }
    else { SegmentSource::Mixed }
}
```

Call `dominant_source(&mic_window, &sys_window)` right before `self.mixer.mix_window(...)` and carry the result to every place the mixed window's VAD segments are re-wrapped into `AudioChunk` (the sites that currently hardcode `device_type: DeviceType::Microphone, // Mixed audio`). Read how the chunk travels to `worker.rs`; extend `AudioChunk` (or the transcription-request struct actually consumed by the worker) with a `segment_source: SegmentSource` field defaulting to `Mixed`, rather than lying through `device_type`. In `worker.rs:211` replace `source: "Audio".to_string()` with the mapping `Mic => "mic"`, `System => "system"`, `Mixed => "mixed"`.
- [ ] **Step 4: Check frontend readers.** `grep -rn '\.source' frontend/src --include='*.ts*' | grep -iv audio_start` and read each hit (known: `types/index.ts`, `TranscriptContext.tsx`, transcript display components). Fix any comparison against the literal `"Audio"`. If the transcript UI displays `source`, map `mic → You`, `system → Them`, `mixed → (nothing)` there.
- [ ] **Step 5: Run tests + full check.** `cargo test -p <pkg> dominant_source` PASS; `cargo check` clean; `cd frontend && npx tsc --noEmit` clean.
- [ ] **Step 6: Commit** `feat: tag transcript segments mic/system before the mixer`.

### Task 2: Assistant module skeleton + settings (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/mod.rs`, `assistant/settings.rs`
- Create: `frontend/src-tauri/migrations/20260901120000_assistant_settings.sql`
- Modify: `frontend/src-tauri/src/lib.rs` (add `mod assistant;`, `.manage(assistant::AssistantHandle::new())`, register commands)
- Test: inline in `settings.rs`

**Interfaces:**
- Produces: `AssistantSettings` struct (all later tasks read it), commands `assistant_get_settings() -> AssistantSettings`, `assistant_save_settings(settings: AssistantSettings)`, `assistant_test_claude() -> ClaudeProbe { ok: bool, version: String, error: Option<String> }`; `assistant::AssistantHandle` managed state (an `Arc<tokio::sync::Mutex<AssistantCore>>` that later tasks fill).

- [ ] **Step 1: Migration** (single-row idiom copied from the settings table):

```sql
CREATE TABLE IF NOT EXISTS assistant_settings (
    id TEXT PRIMARY KEY DEFAULT '1',
    enabled INTEGER NOT NULL DEFAULT 1,
    claude_path TEXT NOT NULL DEFAULT '',
    fast_model TEXT NOT NULL DEFAULT 'claude-sonnet-5',
    fast_effort TEXT NOT NULL DEFAULT 'low',
    deep_model TEXT NOT NULL DEFAULT 'claude-opus-5',
    deep_effort TEXT NOT NULL DEFAULT 'medium',
    trigger_mode TEXT NOT NULL DEFAULT 'gated',
    quiet_gap_secs REAL NOT NULL DEFAULT 2.0,
    names TEXT NOT NULL DEFAULT 'joseph,joe',
    vault_root TEXT NOT NULL DEFAULT '',
    deep_read_dirs TEXT NOT NULL DEFAULT ''
);
INSERT OR IGNORE INTO assistant_settings (id) VALUES ('1');
```

`vault_root` empty means `~/brain/wiki` resolved at use time; `deep_read_dirs` empty means `~/brain`.
- [ ] **Step 2: Failing test** for defaults round-trip in `settings.rs` (sqlx in-memory or the repo's test-db idiom; follow `database/repositories/setting.rs` patterns):

```rust
#[tokio::test]
async fn settings_defaults_load() {
    let pool = test_pool().await; // follow existing repo test helpers; create one if absent
    let s = AssistantSettings::load(&pool).await.unwrap();
    assert!(s.enabled);
    assert_eq!(s.fast_model, "claude-sonnet-5");
    assert_eq!(s.trigger_mode, "gated");
}
```

- [ ] **Step 3: Implement** `AssistantSettings { enabled: bool, claude_path: String, fast_model: String, fast_effort: String, deep_model: String, deep_effort: String, trigger_mode: String, quiet_gap_secs: f64, names: String, vault_root: String, deep_read_dirs: String }` with `load(pool)`/`save(pool)`, `Serialize + Deserialize` (camelCase rename for the wire). Commands in `mod.rs` using `tauri::State<'_, AppState>` for the pool, returning `Result<_, String>`. `assistant_test_claude`: resolve the binary (settings path, else `which claude`, else `~/.local/bin/claude`, `/opt/homebrew/bin/claude`), run `claude --version` with a 10 s timeout, report.
- [ ] **Step 4: Register** in `lib.rs` `generate_handler!` (module-qualified like the neighbors). Run test PASS, `cargo check` clean.
- [ ] **Step 5: Commit** `feat: assistant module skeleton, settings table and commands`.

### Task 3: Claude CLI runner (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/claude_cli.rs`
- Test: inline; plus one env-gated live test

**Interfaces:**
- Produces:

```rust
pub struct ClaudeTurn {
    pub prompt: String, pub cwd: PathBuf,
    pub model: Option<String>, pub effort: Option<String>,
    pub resume: Option<String>, pub fork: bool, pub session_id: Option<String>,
    pub append_system_prompt: Option<String>,
    pub allowed_tools: Vec<String>, pub disallowed_tools: Vec<String>,
    pub partial: bool, pub safe_mode: bool, pub add_dirs: Vec<PathBuf>,
}
pub enum ClaudeEvent { SessionId(String), TextDelta(String), Final(String), Failed(String) }
pub struct ClaudeOutcome { pub session_id: Option<String>, pub text: String, pub error: Option<String> }
// spawn one turn; deltas stream through on_delta; abort via the returned handle
pub async fn run_turn(binary: &Path, turn: ClaudeTurn, on_delta: impl FnMut(&str) + Send,
                      register: impl FnOnce(TurnHandle)) -> ClaudeOutcome;
pub struct TurnHandle { /* kill() terminates the child */ }
```

- Port of `ClaudeCLI.swift` (read it in full). argv order and flags must match it exactly: `-p <prompt> --output-format stream-json --verbose`, then `--safe-mode`, `--add-dir ...`, `--resume`, `--fork-session`, `--session-id`, `--model`, `--effort`, `--include-partial-messages`, `--append-system-prompt`, `--allowedTools <space-joined>`, `--disallowedTools <space-joined>`.

- [ ] **Step 1: Failing tests** for argv building and NDJSON parsing (pure functions, no process):

```rust
#[test]
fn argv_matches_v1_shape() {
    let t = ClaudeTurn { prompt: "hi".into(), safe_mode: true, partial: true,
        resume: Some("abc".into()), fork: true, model: Some("claude-sonnet-5".into()),
        effort: Some("low".into()), ..Default::default() };
    let a = t.argv();
    assert_eq!(&a[..5], &["-p", "hi", "--output-format", "stream-json", "--verbose"]);
    assert!(a.windows(2).any(|w| w == ["--resume", "abc"]));
    assert!(a.contains(&"--fork-session".to_string()));
}
#[test]
fn parses_stream_events() {
    let init = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}"#;
    let result = r#"{"type":"result","result":"Hello","is_error":false}"#;
    assert!(matches!(parse_line(init), Some(ClaudeEvent::SessionId(s)) if s == "s1"));
    assert!(matches!(parse_line(delta), Some(ClaudeEvent::TextDelta(t)) if t == "Hel"));
    assert!(matches!(parse_line(result), Some(ClaudeEvent::Final(t)) if t == "Hello"));
}
```

- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** with `tokio::process::Command` (`.kill_on_drop(true)`, stdin null, stdout/stderr piped, `BufReader::lines()` loop; keep a 1500-char stderr tail; on nonzero exit emit `Failed`). **Step 4:** tests PASS.
- [ ] **Step 5: Env-gated live test** (runs only with `ASSISTANT_CLAUDE_TEST=1`; QA runs it in Task 15):

```rust
#[tokio::test]
#[ignore]
async fn live_one_turn() {
    let bin = resolve_claude_binary(&AssistantSettings::default()).unwrap();
    let out = run_turn(&bin, ClaudeTurn { prompt: "Reply with exactly OK".into(),
        model: Some("claude-haiku-4-5-20251001".into()), partial: false, safe_mode: true,
        disallowed_tools: NO_TOOLS.iter().map(|s| s.to_string()).collect(),
        cwd: std::env::temp_dir(), ..Default::default() }, |_| {}, |_| {}).await;
    assert!(out.error.is_none(), "{:?}", out.error);
    assert!(out.text.contains("OK"));
}
```

- [ ] **Step 6: Commit** `feat: claude CLI turn runner with stream-json parsing`.

### Task 4: Transcript log + utterance assembler (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/transcript.rs`
- Test: inline

**Interfaces:**
- Consumes: `TranscriptUpdate` (from `audio/transcription/worker.rs`; fields `text, timestamp, source, sequence_id, is_partial, ...`).
- Produces:

```rust
pub enum Speaker { You, Them, Unknown }          // mic/system/mixed
pub struct TranscriptLog { /* ordered, labeled, cursor-addressable */ }
impl TranscriptLog {
    pub fn ingest(&mut self, u: &TranscriptUpdate) -> Vec<AssemblerOut>;
    pub fn delta_since(&self, cursor: usize) -> (String, usize); // labeled "You:/Them:" text + new cursor
    pub fn window(&self, seconds: f64, speaker: Option<Speaker>) -> String;
    pub fn all(&self) -> String;
}
pub enum AssemblerOut {
    Running { speaker: Speaker, text: String },      // partial or still-open utterance grew
    Utterance { speaker: Speaker, text: String },    // closed: quiet gap >= 1.2 s or speaker flip
}
```

- [ ] **Step 1: Failing tests**: consecutive same-speaker segments within 1.2 s merge into one utterance; a speaker flip closes the open utterance; `is_partial` updates produce `Running` and are replaced (never appended) by the final segment with the same/succeeding `sequence_id`; `delta_since` returns only new finals with `You:`/`Them:` labels; `window(15.0, Some(Them))` returns only recent Them text. Use handcrafted `TranscriptUpdate` values with explicit `audio_start_time`/`audio_end_time`.
- [ ] **Step 2:** FAIL. **Step 3: Implement** (keep finals in a `Vec<(Speaker, String, f64 /*end time*/)>`; cursor = index; closing is driven by comparing the incoming segment's `audio_start_time` to the open utterance's end time). **Step 4:** PASS. `cargo check` clean.
- [ ] **Step 5: Commit** `feat: assistant transcript log and utterance assembler`.

### Task 5: Trigger engine port (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/trigger.rs`
- Test: inline

**Interfaces:**
- Consumes: `AssemblerOut`, `Speaker` (Task 4).
- Produces:

```rust
pub enum TriggerMode { Manual, Gated, Continuous }
pub enum TriggerReason { NameCalled, Question }
pub struct TriggerEngine { pub on_fire: Box<dyn Fn(String, TriggerReason) + Send> }
impl TriggerEngine {
    pub fn update(&mut self, mode: TriggerMode, quiet_gap: f64, listening: bool, names: Vec<String>);
    pub fn consume_running(&mut self, speaker: Speaker, text: &str);
    pub fn consume_utterance(&mut self, speaker: Speaker, text: &str); // schedules gap via tokio task
    pub fn reset(&mut self);
}
```

- [ ] **Step 1:** Read `TriggerEngine.swift` in full and port it exactly: substantive = at least 2 words of 2+ chars; repeat guard = 0.7 content-word overlap against last 5 fires (content words = lowercased, stopwords stripped; port the stopword list from `CardFormat.swift`'s `contentWords`); question shape = `?` or interrogative openers/fillers/lead-ins/ask-phrases (copy the four word lists verbatim); name-call arms mid-utterance and fires early only on a complete-looking thought (port `UtteranceAssembler.looksComplete` from `app/Sources/MeetingAssistant/Engine/UtteranceAssembler.swift`); gated questions wait `quiet_gap` measured from utterance end, restarted by any new speech. Only Them utterances trigger; Unknown never triggers.
- [ ] **Step 2: Failing tests** (write before implementing; these encode v1's fixed bugs):

```rust
#[test] fn noise_marks_never_fire() { /* "?", ".", "3.", "Q." -> no fire */ }
#[test] fn short_question_all_stopwords_fires() { /* "So what does that do to the Q4 migration plan?" fires */ }
#[test] fn revised_drafts_of_one_question_fire_once() {
    /* three revisions sharing >=70% content words -> exactly one fire */ }
#[test] fn name_fires_even_without_question_shape() { /* "Joseph, walk us through the rollout" fires NameCalled */ }
#[test] fn you_channel_never_triggers() { /* Speaker::You question -> no fire */ }
#[test] fn manual_mode_never_volunteers() {}
```

- [ ] **Step 3:** FAIL → implement → PASS. **Step 4: Commit** `feat: trigger engine port from v1`.

### Task 6: Card format (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/card.rs`
- Test: inline

**Interfaces:**
- Produces:

```rust
pub struct ParsedCard { pub lead: String, pub bullets: Vec<String>, pub source: String,
                        pub is_skip: bool, pub is_empty: bool }
pub fn parse(text: &str) -> ParsedCard;          // LEAD:/bullets/- /SOURCE: contract, SKIP sentinel
pub fn can_decide(streamed: &str) -> bool;       // hold first tokens until SKIP is ruled out
pub fn contradicts(fast: &ParsedCard, deep: &ParsedCard) -> bool;
pub fn changed_bullets(old: &ParsedCard, new: &ParsedCard) -> Vec<String>;
pub fn content_words(text: &str) -> std::collections::HashSet<String>; // shared with trigger.rs
```

- [ ] **Step 1:** Read `CardFormat.swift` in full; port parsing, `canDecide`, `contradicts`, `changedBullets`, `contentWords` and the stopword list verbatim.
- [ ] **Step 2: Failing tests**: parses a well-formed card; tolerates missing SOURCE; `SKIP` (exact, and with trailing whitespace) → `is_skip`; `can_decide("S")` false, `can_decide("SKIP")` true-and-skip, `can_decide("LEAD: x")` true; contradiction on a changed lead, not on a reworded bullet (use `CardFormat.swift`'s own threshold logic as ground truth).
- [ ] **Step 3:** implement → PASS. **Step 4: Commit** `feat: card format parser and contradiction check`.

### Task 7: Answer lanes + prompts (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/lanes.rs`, `assistant/prompts/fast.md`, `assistant/prompts/deep.md`
- Test: inline (logic only; no live spawns)

**Interfaces:**
- Consumes: `run_turn`/`ClaudeTurn` (Task 3), `TranscriptLog` (Task 4), card fns (Task 6), `AssistantSettings` (Task 2).
- Produces:

```rust
pub struct AnswerLanes { /* trunk/fast/deep ids, cursors, in-flight flags, fast drafts, live handles */ }
pub enum CardKind { Answer, Ask, Explain, Catchup }
pub struct CardOut { pub id: String, pub kind: CardKind, pub question: String,
    pub lead: String, pub bullets: Vec<String>, pub source: String,
    pub phase: String /* drafting|checked|corrected */, pub changed_lines: Vec<String>, pub ts: i64 }
impl AnswerLanes {
    pub async fn open(&mut self, seed: String, cfg: &LaneConfig, emit: EmitFn); // trunk + 2 forks
    pub fn answer(&mut self, question: String, kind: CardKind, log: &TranscriptLog, emit: EmitFn);
    pub fn explain(&mut self, log: &TranscriptLog, emit: EmitFn);
    pub fn catchup(&mut self, log: &TranscriptLog, emit: EmitFn);
    pub async fn draft_note(&mut self, transcript: String, qa_log: String) -> Result<String, String>;
    pub fn close(&mut self);
}
pub type EmitFn = std::sync::Arc<dyn Fn(CardOut) + Send + Sync>;
```

- [ ] **Step 1: Prompts.** Copy `prompts/fast.md` and `prompts/deep.md` from the v1 repo verbatim, then apply exactly two content edits in each: (a) replace mentions of "the Context Pack you were given" with "the meeting brief you were given, if any"; (b) leave the channel section as is (`You:`/`Them:` labels match Task 4's output). Embed with `include_str!("prompts/fast.md")`.
- [ ] **Step 2:** Read `AnswerLanes.swift` in full and port the flow: trunk seed prompt ("This is the brief for the meeting that is about to start... Reply with exactly the word READY."), two `--fork-session` READY forks; per-turn `--resume <lane-id>`; fork-instead-of-queue when a lane is in flight (a fork does not advance the cursor); fast lane no tools (`NO_TOOLS` list from v1), deep lane `allowed = ["Read","Grep","Glob"]`, `disallowed = write set`, `add_dirs = deep_read_dirs`; delta prompt shape `"NEW TRANSCRIPT since your last turn:\n{delta}\n\nTRIGGER: {q}\n\nAnswer in the card format, or reply SKIP."`; explain = fast-only over `window(15s, Them)`; catch-up = first press whole meeting then 5-minute window, fast-only, lands checked; deep settle replaces the card, `corrected` phase + changed lines on contradiction; deep SKIP leaves the fast draft standing (log it); fast SKIP + deep answer presents the deep card alone.
- [ ] **Step 3: Failing tests** for the pure parts: cursor advance/fork rules (simulate two rapid answers; second must fork and not advance), settle-state transitions (fast draft → deep checked; contradiction → corrected with changed lines; deep SKIP → draft stands). Inject a fake runner (`type Runner = Arc<dyn Fn(ClaudeTurn) -> ... >`) so tests never spawn.
- [ ] **Step 4:** implement → PASS. `cargo check` clean. **Step 5: Commit** `feat: answer lanes with fast/deep fork flow and prompts`.

### Task 8: Voice ask (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/voice_ask.rs`
- Test: inline

**Interfaces:**
- Consumes: `AssemblerOut` mic events (Task 4).
- Produces:

```rust
pub struct VoiceAsk { pub on_submit: Box<dyn Fn(String) + Send>,
                      pub publish: Box<dyn Fn(VoiceState) + Send> }
pub enum VoiceState { Off, Listening { heard: String }, Submitting { question: String } }
impl VoiceAsk {
    pub fn start(&mut self);            // idempotent while capturing
    pub fn finish(&mut self);           // submits what was heard
    pub fn cancel(&mut self);
    pub fn note_running(&mut self, text: &str);   // replaces, never appends
    pub fn note_utterance(&mut self, text: &str); // settles into the list
}
```

- [ ] **Step 1:** Read `VoiceAsk.swift` in full; port: silence gap 1.5 s auto-submit (armed only by speech, restarted per scrap), 45 s cap, submit linger 1.4 s then `Off`, empty capture cancels silently, drafts replace / utterances append.
- [ ] **Step 2: Failing tests**: two settled utterances + one running join in spoken order; finish with nothing heard publishes `Off` and never calls `on_submit`; a second `start()` during capture is a no-op. (Timers: use `tokio::time::pause()` test clock.)
- [ ] **Step 3:** implement → PASS. **Step 4: Commit** `feat: voice ask capture over mic transcript`.

### Task 9: Note drafting and vault save (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/note.rs`, `assistant/prompts/note.md`
- Test: inline

**Interfaces:**
- Consumes: `AnswerLanes::draft_note` (Task 7), `AssistantSettings.vault_root`.
- Produces:

```rust
pub struct NoteDraft { pub slug: String, pub markdown: String }
pub fn parse_note(raw: &str) -> Result<NoteDraft, String>;   // === SLUG === / === NOTE === markers
pub fn save_note(vault_root: &Path, date: &str, draft: &NoteDraft, transcript: &str,
                 qa_log: &str, dry_run: bool) -> Result<Vec<PathBuf>, String>;
```

- [ ] **Step 1: Prompt.** Copy v1 `prompts/note.md`, remove the `=== LINE ===` section and the sentence describing it (no linked project pages in v2); output contract becomes SLUG + NOTE only.
- [ ] **Step 2: Failing tests**: `parse_note` extracts slug + body and rejects a missing marker; `save_note` writes `<root>/_sources/meetings/2026-09-01-<slug>.md` (header block with date/title + note body + `## Q&A log` section from `qa_log`) and `..._transcript.md`, creates dirs, and with `dry_run` writes nothing and returns the would-be paths. Use a tempdir.
- [ ] **Step 3:** implement → PASS. Git commit of the vault happens in `save_note` (non-dry-run) via `std::process::Command` `git -C <root> add <files> && git commit -m "Meeting note: <slug>"`; failures are returned, never panic. **Step 4: Commit** `feat: note drafting parse and vault save`.

### Task 10: Assistant core wiring, commands and events (backend lane)

**Files:**
- Create: `frontend/src-tauri/src/assistant/core.rs`, `assistant/commands.rs`
- Modify: `frontend/src-tauri/src/lib.rs` (register the command list; add the `setup` hook listeners)
- Test: inline logic tests; live behavior lands in Task 15

**Interfaces:**
- Consumes: everything above.
- Produces (the frontend contract, spec section "Commands and events", verbatim):
  - Commands: `assistant_get_state`, `assistant_set_enabled(enabled: bool)`, `assistant_ask(text: String)`, `assistant_explain`, `assistant_catchup`, `assistant_set_mode(mode: String)`, `assistant_set_listening(listening: bool)`, `assistant_voice_start`, `assistant_voice_finish`, `assistant_voice_cancel`, `assistant_draft_note`, `assistant_save_note`, `assistant_discard_note`, `assistant_set_brief(text: String)`, plus Task 2's three.
  - Events: `assistant-card` (`CardOut`, camelCase), `assistant-status` (`{ enabled, sessionOpen, lanesReady, mode, listening, claudeOk, lastError }`), `assistant-voice` (`{ state, heard }`), `assistant-note` (`{ state, markdown, error }`).

- [ ] **Step 1:** `core.rs`: `AssistantCore` owns `TranscriptLog`, `TriggerEngine`, `AnswerLanes`, `VoiceAsk`, note state, settings snapshot. In the tauri `setup` hook (`lib.rs`), `app.listen("transcript-update", ...)` deserializes the payload and forwards to `AssistantHandle` (spawn a tokio task; never block the event thread), `app.listen("recording-started", ...)` opens the session when enabled (seed = meeting title + date + optional brief typed in the panel before start), and listen for the stop path: read `audio/recording_commands.rs` around the stop command to find the emitted stop/complete event; bind session close + note availability to it. Voice ask consumes mic `Running`/`Utterance` events while capturing; trigger engine consumes Them events; every card/status/voice/note change emits its event.
- [ ] **Step 2:** Wire `on_fire` → `lanes.answer(question, CardKind::Answer, ...)`; `assistant_ask` → `CardKind::Ask` (never SKIPs by prompt: append "This question was asked by Joseph directly. Do not reply SKIP." to the turn prompt); voice submit → same path as `assistant_ask`.
- [ ] **Step 3: Tests**: `assistant_set_mode("continuous")` round-trips through `assistant_get_state`; a transcript event arriving while disabled changes nothing; a lane failure sets `lastError` and `lanesReady` stays truthful.
- [ ] **Step 4:** `cargo check` + all `cargo test` green. **Step 5: Commit** `feat: assistant core, commands and event wiring`.

### Task 11: Frontend types, service and context (frontend lane)

**Files:**
- Create: `frontend/src/services/assistantService.ts`, `frontend/src/contexts/AssistantContext.tsx`
- Modify: `frontend/src/types/index.ts` (append assistant types), `frontend/src/app/layout.tsx` (mount provider beside the existing ones)

**Interfaces:**
- Consumes: the pinned contract only (spec "Commands and events"). Types, exactly:

```ts
export type AssistantCardPhase = 'drafting' | 'checked' | 'corrected';
export type AssistantCardKind = 'answer' | 'ask' | 'explain' | 'catchup';
export interface AssistantCard { id: string; kind: AssistantCardKind; question: string;
  lead: string; bullets: string[]; source: string; phase: AssistantCardPhase;
  changedLines: string[]; ts: number; }
export interface AssistantStatus { enabled: boolean; sessionOpen: boolean; lanesReady: boolean;
  mode: 'manual' | 'gated' | 'continuous'; listening: boolean; claudeOk: boolean;
  lastError: string | null; }
export interface AssistantVoice { state: 'off' | 'listening' | 'submitting'; heard: string; }
export interface AssistantNote { state: 'idle' | 'drafting' | 'ready' | 'saved' | 'failed';
  markdown: string; error: string | null; }
```

- Produces: `useAssistant()` hook returning `{ status, cards, voice, note, ask(text), explain(), catchup(), setMode(m), setListening(b), setEnabled(b), voiceStart(), voiceFinish(), voiceCancel(), draftNote(), saveNote(), discardNote() }`.

- [ ] **Step 1:** `assistantService.ts` wraps `invoke` + `listen` exactly like `transcriptService.ts` does (read it first; same unlisten/cleanup discipline). Card events upsert by `id` (replace in place; newest first for inserts).
- [ ] **Step 2:** `AssistantContext.tsx` modeled on the slimmer existing contexts; provider added in `layout.tsx` inside the existing provider stack.
- [ ] **Step 3:** `cd frontend && npx tsc --noEmit` clean; `pnpm run lint` clean for the new files. **Step 4: Commit** `feat: assistant frontend service, types and context`.

### Task 12: AssistantPanel UI (frontend lane)

**Files:**
- Create: `frontend/src/components/AssistantPanel/index.tsx`, `AssistantCard.tsx`, `AskBox.tsx`, `NoteBar.tsx`
- Modify: `frontend/src/app/page.tsx` (mount as sibling of `TranscriptPanel` in the flex row; relax `TranscriptPanel`'s width so the two share it), `frontend/src/app/_components/TranscriptPanel.tsx` (width classes only)

**Interfaces:**
- Consumes: `useAssistant()` (Task 11), shadcn primitives in `components/ui/*`, Tailwind idiom of the surrounding files.

- [ ] **Step 1:** Panel layout: right-side column (`w-[380px]` class range, collapsible to a slim toggle rail via local state): header row (listening dot button, mode chip button cycling manual→gated→continuous, on/off switch), scrollable card stack (current card first), `AskBox` pinned at the bottom (text input + send, mic button showing `voice.state`, live `heard` line under it while listening), `Explain` and `Catch up` buttons above the ask box, `NoteBar` shown when `!status.sessionOpen && cards.length > 0` or `note.state !== 'idle'` (Draft note → drafting spinner → markdown preview in a scroll area → Save / Discard). While recording is idle and no session is open, show a small optional "Meeting brief" textarea above the card stack; the frontend sends it via a `assistant_set_brief(text: String)` command (add `setBrief(text)` to the Task 11 context contract; Task 10 stores it and folds it into the trunk seed when `recording-started` opens the session, clearing it after).
- [ ] **Step 2:** `AssistantCard.tsx`: lead line semibold, up to 3 bullets, small mono source line, timestamp. Phase styles: `drafting` amber left border + subtle pulse; `checked` neutral; `corrected` a visible "corrected" badge and `changedLines` briefly highlighted (CSS transition, no timers beyond one `setTimeout`). Match the app's existing card/border/rounding idiom (rounded, not square; never childish).
- [ ] **Step 3:** Empty states: assistant disabled (one line + enable button), `claudeOk === false` (one line + "Open Settings" link to `/settings`), lanes warming (skeleton shimmer), no cards yet (quiet hint listing the hotkeys).
- [ ] **Step 4:** `npx tsc --noEmit` + lint clean; `pnpm run dev` renders the page with the panel (mock: temporarily flip provider state defaults, then revert). **Step 5: Commit** `feat: assistant panel UI`.

### Task 13: Assistant settings tab (frontend lane)

**Files:**
- Create: `frontend/src/components/AssistantSettings.tsx`
- Modify: `frontend/src/app/settings/page.tsx` (add `{ value: 'assistant', label: 'Assistant' }` to `TABS` + `TabsContent`)

- [ ] **Step 1:** Model the component on `SummaryModelSettings.tsx` (read it first): loads via `assistant_get_settings`, saves via `assistant_save_settings`, fields: enabled switch; claude path text input with a "Test claude" button rendering `assistant_test_claude`'s result (version or error, inline); fast/deep model + effort selects (models: claude-sonnet-5, claude-opus-5, claude-haiku-4-5-20251001; efforts: low, medium, high); default trigger mode select; quiet gap number input (seconds, step 0.5); names text input (comma-separated, help text "Names the room calls you"); vault root path input.
- [ ] **Step 2:** `npx tsc --noEmit` + lint clean; settings page renders the tab. **Step 3: Commit** `feat: assistant settings tab`.

### Task 14: Global hotkeys (after both lanes)

**Files:**
- Modify: `frontend/src-tauri/Cargo.toml` (add `tauri-plugin-global-shortcut = "2"`), `frontend/src-tauri/capabilities/*.json` if the repo gates plugins there (read the existing capability files), `frontend/src-tauri/src/lib.rs` (plugin init + handlers)

- [ ] **Step 1:** Register Option-A (voice ask start/finish toggle), Option-E (explain), Option-C (catch-up), Option-M (cycle mode) in the plugin's handler, each calling the same core functions as the commands; Escape cancels voice ask only while capturing (in-window key handler in `AssistantPanel`, not a global grab). Registration failure: log + set nothing; the buttons cover every action.
- [ ] **Step 2:** `cargo check` clean; dev run: press each hotkey with the app unfocused, watch the panel react. **Step 3: Commit** `feat: global hotkeys for assistant actions`.

### Task 15: End-to-end verification (QA, read-only + running the app)

No file changes. Protocol:

- [ ] **Step 1:** `cd frontend && pnpm install && pnpm run tauri:dev` (Metal machine; `tauri-auto.js` picks the feature). First launch: complete onboarding, let the Parakeet model download (~670 MB).
- [ ] **Step 2:** `ASSISTANT_CLAUDE_TEST=1 cargo test -p <pkg> live_one_turn -- --ignored` passes (subscription auth works from a spawned process).
- [ ] **Step 3:** Full `cargo test` and `npx tsc --noEmit` green.
- [ ] **Step 4:** Real flow, as a user hits it: start a recording; speak into the mic AND play a spoken clip aloud (`say -o /tmp/q.aiff "Joseph, what does the assistant do when the room asks a question?" && afplay /tmp/q.aiff`); verify: transcript shows both, mic segments labeled You and playback segments labeled Them (or Unknown when ambiguous, without breaking anything); the name-call fires a card that streams amber and settles; a typed ask streams and settles; Explain and Catch-up render checked cards; ⌥A voice ask captures, shows heard text, auto-submits after silence; stop recording; Draft note produces a preview; Save with `ASSISTANT_DRY_RUN=1` reports paths and writes nothing.
- [ ] **Step 5:** Screenshot the panel in: drafting, checked, corrected (force one by asking a question, then a contradicting follow-up if none occurs naturally; otherwise verify the style with a state-injected story render), voice listening, note preview. Verdict WORKS / PARTIAL / BROKEN with evidence; the owning lane fixes, QA re-verifies.
