// assistant/core.rs
//
// The assistant's live session: transcript log, trigger engine, answer
// lanes, voice ask, note state. Recording and transcription never wait on,
// or fail because of, any of this: every entry point here locks its own
// state, does its own work, and reports failure through `last_error` or a
// note's `error` field rather than propagating into the audio path.
//
// Everything below `install` is a plain async function taking `&AssistantHandle`
// rather than a Tauri `State`, so it is callable directly from a unit test
// with no running app. The `#[tauri::command]` wrappers in `commands.rs` are
// thin one-line adapters over these.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Listener, Manager, Wry};

use crate::audio::TranscriptUpdate;

use super::lanes::{claude_runner, AnswerLanes, CardKind, CardOut, LaneConfig, Runner};
use super::note::{self, NoteDraft, NOTE_PROMPT};
use super::settings::{self, AssistantSettings};
use super::transcript::{AssemblerOut, Speaker, TranscriptLog};
use super::trigger::{TriggerEngine, TriggerMode};
use super::voice_ask::{VoiceAsk, VoiceState};
use super::AssistantHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteState {
    Idle,
    Drafting,
    Ready,
    Saved,
    Failed,
}

impl NoteState {
    fn label(self) -> &'static str {
        match self {
            NoteState::Idle => "idle",
            NoteState::Drafting => "drafting",
            NoteState::Ready => "ready",
            NoteState::Saved => "saved",
            NoteState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusOut {
    pub enabled: bool,
    pub session_open: bool,
    pub lanes_ready: bool,
    pub mode: String,
    pub listening: bool,
    pub claude_ok: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct VoiceOut {
    state: &'static str,
    heard: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NoteOut {
    state: &'static str,
    markdown: String,
    error: Option<String>,
}

pub struct AssistantCore {
    settings: AssistantSettings,
    transcript: TranscriptLog,
    trigger: TriggerEngine,
    lanes: AnswerLanes,
    voice: VoiceAsk,
    /// The open You-utterance's text at the moment voice-ask's hotkey was
    /// pressed, if any. Stripped as a prefix from transcript text fed to
    /// `voice` so speech from before the press does not bleed into the
    /// captured question.
    voice_baseline: String,

    app: Option<AppHandle<Wry>>,
    /// Built once in `install`; every settled card reaches the panel and,
    /// for an answer or a direct ask, appends to `qa_log` for the note.
    card_emit: super::lanes::EmitFn,
    qa_log: Arc<StdMutex<Vec<String>>>,

    enabled: bool,
    session_open: bool,
    lanes_ready: bool,
    claude_ok: bool,
    last_error: Option<String>,
    mode: TriggerMode,
    listening: bool,

    /// Typed in the panel before a recording starts; folded into the next
    /// session's seed and cleared once used.
    pending_brief: Option<String>,

    note_state: NoteState,
    note_draft: Option<NoteDraft>,
    note_markdown: String,
    note_error: Option<String>,
    session_transcript: String,

    /// Edge-triggered so `on_transcript_update`'s drop condition logs once
    /// per state change instead of once per transcript chunk.
    transcript_ingest_ok: bool,
}

impl Default for AssistantCore {
    fn default() -> Self {
        Self {
            settings: AssistantSettings::default(),
            transcript: TranscriptLog::default(),
            trigger: TriggerEngine::new(|_, _| {}),
            // Replaced with a real binary-backed runner once a session opens;
            // an empty path just fails fast if ever invoked before that.
            lanes: AnswerLanes::new(claude_runner(PathBuf::new())),
            voice: VoiceAsk::new(|_| {}, |_| {}),
            voice_baseline: String::new(),
            app: None,
            card_emit: Arc::new(|_| {}),
            qa_log: Arc::new(StdMutex::new(Vec::new())),
            enabled: true,
            session_open: false,
            lanes_ready: false,
            claude_ok: false,
            last_error: None,
            mode: TriggerMode::Gated,
            listening: true,
            pending_brief: None,
            note_state: NoteState::Idle,
            note_draft: None,
            note_markdown: String::new(),
            note_error: None,
            session_transcript: String::new(),
            transcript_ingest_ok: false,
        }
    }
}

impl AssistantCore {
    fn status_snapshot(&self) -> StatusOut {
        StatusOut {
            enabled: self.enabled,
            session_open: self.session_open,
            lanes_ready: self.lanes_ready,
            mode: mode_label(self.mode).to_string(),
            listening: self.listening,
            claude_ok: self.claude_ok,
            last_error: self.last_error.clone(),
        }
    }

    fn emit_status(&self) {
        let Some(app) = &self.app else { return };
        if let Err(e) = app.emit("assistant-status", &self.status_snapshot()) {
            log::warn!("assistant: failed to emit assistant-status: {}", e);
        }
    }

    fn emit_note(&self) {
        let Some(app) = &self.app else { return };
        let out = NoteOut {
            state: self.note_state.label(),
            markdown: self.note_markdown.clone(),
            error: self.note_error.clone(),
        };
        if let Err(e) = app.emit("assistant-note", &out) {
            log::warn!("assistant: failed to emit assistant-note: {}", e);
        }
    }
}

fn mode_label(mode: TriggerMode) -> &'static str {
    match mode {
        TriggerMode::Manual => "manual",
        TriggerMode::Gated => "gated",
        TriggerMode::Continuous => "continuous",
    }
}

fn parse_mode(raw: &str) -> Result<TriggerMode, String> {
    match raw {
        "manual" => Ok(TriggerMode::Manual),
        "gated" => Ok(TriggerMode::Gated),
        "continuous" => Ok(TriggerMode::Continuous),
        other => Err(format!("unknown assistant mode: {}", other)),
    }
}

fn split_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Empty means `~/brain/wiki`, resolved here rather than at save time so a
/// missing home directory fails the same way settings validation would.
fn resolve_vault_root(raw: &str) -> PathBuf {
    if raw.trim().is_empty() {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join("brain")
            .join("wiki")
    } else {
        PathBuf::from(raw.trim())
    }
}

/// Empty means `[~/brain]`, matching `AssistantSettings::deep_read_dirs`'s
/// doc comment.
fn resolve_deep_read_dirs(raw: &str) -> Vec<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        return vec![home.join("brain")];
    }
    trimmed
        .split(',')
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Builds the shared card emitter once, at `install` time. A settled answer
/// or ask card also appends to `qa_log` so the end-of-meeting note has a
/// record of what was asked and answered; a drafting card, explain, or
/// catch-up does not.
fn make_card_emit(app: AppHandle<Wry>, qa_log: Arc<StdMutex<Vec<String>>>) -> super::lanes::EmitFn {
    Arc::new(move |card: CardOut| {
        if matches!(card.kind, CardKind::Answer | CardKind::Ask)
            && (card.phase == "checked" || card.phase == "corrected")
        {
            qa_log
                .lock()
                .unwrap()
                .push(format!("Q: {}\nA: {}", card.question, card.lead));
        }
        if let Err(e) = app.emit("assistant-card", &card) {
            log::warn!("assistant: failed to emit assistant-card: {}", e);
        }
    })
}

fn make_voice_publish(app: AppHandle<Wry>) -> Arc<dyn Fn(VoiceState) + Send + Sync> {
    Arc::new(move |state: VoiceState| {
        let (label, heard) = match state {
            VoiceState::Off => ("off", String::new()),
            VoiceState::Listening { heard } => ("listening", heard),
            VoiceState::Submitting { question } => ("submitting", question),
        };
        if let Err(e) = app.emit("assistant-voice", &VoiceOut { state: label, heard }) {
            log::warn!("assistant: failed to emit assistant-voice: {}", e);
        }
    })
}

// ---------------------------------------------------------------------
// Entry points. Each locks AssistantCore, does its work, and returns; the
// real claude turns run on tasks spawned inside AnswerLanes/TriggerEngine/
// VoiceAsk themselves.
// ---------------------------------------------------------------------

pub async fn get_state(handle: &AssistantHandle) -> StatusOut {
    handle.0.lock().await.status_snapshot()
}

pub async fn set_enabled(handle: &AssistantHandle, enabled: bool) {
    let (already_open, app) = {
        let mut core = handle.0.lock().await;
        core.enabled = enabled;
        if !enabled {
            core.lanes.close();
            core.voice.cancel();
            // Otherwise every other path (`ask`/`explain`/`catchup`,
            // the panel's own `actionsDisabled`) disagrees with reality:
            // lanes are closed but session_open still claims a live
            // session, and explain/catchup have no `enabled` guard of
            // their own to catch it.
            core.session_open = false;
        }
        core.emit_status();
        (core.session_open, core.app.clone())
    };

    // The panel switch is the user's explicit intent, not a transient
    // runtime toggle -- persist it so the next recording's `open_session`
    // (which reloads settings fresh from the DB) doesn't read a stale row
    // and silently revert the choice just made.
    if let Some(state) = app.as_ref().and_then(|a| a.try_state::<crate::state::AppState>()) {
        if let Err(e) = settings::AssistantSettings::persist_enabled(state.db_manager.pool(), enabled).await {
            log::warn!("assistant: failed to persist enabled={}: {}", enabled, e);
        }
    }

    if !enabled || already_open {
        return;
    }
    // The session only opens on `recording-started`. If a recording was
    // already running when the assistant got turned on here, that event
    // already fired and will not fire again -- without this, the
    // assistant stays closed for the rest of the recording no matter how
    // many times the switch is flipped.
    if crate::audio::recording_commands::is_recording().await {
        log::info!("assistant: enabled while a recording is already running, opening the session now");
        open_session(handle, Some(true)).await;
    }
}

/// Shared by `set_mode` (explicit, from the settings UI) and `cycle_mode`
/// (the Option-M hotkey): applies a mode, rebuilds the trigger engine's
/// config from it, and emits status. Caller already holds the lock.
fn apply_mode(core: &mut AssistantCore, mode: TriggerMode) {
    core.mode = mode;
    let names = split_names(&core.settings.names);
    let quiet_gap = core.settings.quiet_gap_secs;
    let listening = core.listening;
    core.trigger.update(mode, quiet_gap, listening, names);
    core.emit_status();
}

pub async fn set_mode(handle: &AssistantHandle, mode: &str) -> Result<(), String> {
    let parsed = parse_mode(mode)?;
    let mut core = handle.0.lock().await;
    apply_mode(&mut core, parsed);
    Ok(())
}

/// Option-M: manual -> gated -> continuous -> manual.
pub async fn cycle_mode(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    let next = match core.mode {
        TriggerMode::Manual => TriggerMode::Gated,
        TriggerMode::Gated => TriggerMode::Continuous,
        TriggerMode::Continuous => TriggerMode::Manual,
    };
    apply_mode(&mut core, next);
}

pub async fn set_listening(handle: &AssistantHandle, listening: bool) {
    let mut core = handle.0.lock().await;
    core.listening = listening;
    let names = split_names(&core.settings.names);
    let mode = core.mode;
    let quiet_gap = core.settings.quiet_gap_secs;
    core.trigger.update(mode, quiet_gap, listening, names);
    core.emit_status();
}

pub async fn set_brief(handle: &AssistantHandle, text: String) {
    let mut core = handle.0.lock().await;
    core.pending_brief = if text.trim().is_empty() { None } else { Some(text) };
}

/// Surfaces "the session is not open" as a visible error rather than a
/// silent no-op. The panel renders `lastError`, so a button press that gets
/// dropped needs to reach it the same way a lane failure does -- otherwise
/// pressing Explain, Catch up, or Ask while closed just looks like nothing
/// happened.
fn drop_closed_session(core: &mut AssistantCore, action: &str) {
    log::info!("assistant: {} dropped, no open session", action);
    core.last_error = Some("Assistant session is not open yet.".to_string());
    core.emit_status();
}

/// Same shape as `drop_closed_session`, for the other reason a press can be
/// dropped: the session is open but the assistant itself is off. Every
/// entry point checks `session_open` before `enabled` -- a closed session
/// gets the closed-session message even if the assistant also happens to
/// be off, since that is the more specific and more actionable of the two.
fn drop_disabled(core: &mut AssistantCore, action: &str) {
    log::debug!("assistant: {} dropped, assistant disabled", action);
    core.last_error = Some("Assistant is turned off.".to_string());
    core.emit_status();
}

/// Fires on a trigger, a typed ask, and a submitted voice ask alike. `extra`
/// reaches the model's prompt only, never the card's displayed question.
pub async fn ask(handle: &AssistantHandle, question: String, kind: CardKind, extra: Option<&str>) {
    let mut core = handle.0.lock().await;
    if !core.session_open {
        drop_closed_session(&mut core, "ask");
        return;
    }
    if !core.enabled {
        drop_disabled(&mut core, "ask");
        return;
    }
    let emit = core.card_emit.clone();
    let AssistantCore { lanes, transcript, .. } = &mut *core;
    lanes.answer_with_note(question, extra, kind, transcript, emit);
}

pub async fn explain(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    if !core.session_open {
        drop_closed_session(&mut core, "explain");
        return;
    }
    if !core.enabled {
        drop_disabled(&mut core, "explain");
        return;
    }
    let emit = core.card_emit.clone();
    let AssistantCore { lanes, transcript, .. } = &mut *core;
    let result = lanes.explain(transcript, emit);
    if let Err(e) = result {
        core.last_error = Some(e);
        core.emit_status();
    }
}

pub async fn catchup(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    if !core.session_open {
        drop_closed_session(&mut core, "catch-up");
        return;
    }
    if !core.enabled {
        drop_disabled(&mut core, "catch-up");
        return;
    }
    let emit = core.card_emit.clone();
    let AssistantCore { lanes, transcript, .. } = &mut *core;
    lanes.catchup(transcript, emit);
}

pub async fn voice_start(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    core.voice_baseline = core.transcript.open_you_preview();
    core.voice.start();
}

pub async fn voice_finish(handle: &AssistantHandle) {
    handle.0.lock().await.voice.finish();
}

pub async fn voice_cancel(handle: &AssistantHandle) {
    handle.0.lock().await.voice.cancel();
}

/// Option-A: start if idle, finish if capturing. Checked and acted on under
/// one lock so two presses close together can't both see "idle".
pub async fn voice_toggle(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    if core.voice.is_capturing() {
        core.voice.finish();
    } else {
        core.voice_baseline = core.transcript.open_you_preview();
        core.voice.start();
    }
}

pub async fn draft_note(handle: &AssistantHandle) -> Result<(), String> {
    let (transcript_text, qa_log_text) = {
        let mut core = handle.0.lock().await;
        if core.session_open {
            return Err("stop the recording before drafting a note".to_string());
        }
        core.note_state = NoteState::Drafting;
        core.note_error = None;
        core.emit_note();
        let transcript_text = core.transcript.all();
        let qa_log_text = core.qa_log.lock().unwrap().join("\n\n");
        core.session_transcript = transcript_text.clone();
        (transcript_text, qa_log_text)
    };

    let result = {
        let mut core = handle.0.lock().await;
        core.lanes.draft_note(transcript_text, qa_log_text).await
    };

    let mut core = handle.0.lock().await;
    match result {
        Ok(raw) => match note::parse_note(&raw) {
            Ok(draft) => {
                core.note_markdown = draft.markdown.clone();
                core.note_draft = Some(draft);
                core.note_state = NoteState::Ready;
                core.note_error = None;
            }
            Err(e) => {
                core.note_state = NoteState::Failed;
                core.note_error = Some(e);
            }
        },
        Err(e) => {
            core.note_state = NoteState::Failed;
            core.note_error = Some(e);
        }
    }
    core.emit_note();
    Ok(())
}

pub async fn save_note(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    let Some(draft) = core.note_draft.clone() else {
        core.note_state = NoteState::Failed;
        core.note_error = Some("no note draft to save".to_string());
        core.emit_note();
        return;
    };
    let vault_root = resolve_vault_root(&core.settings.vault_root);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dry_run = std::env::var("ASSISTANT_DRY_RUN").ok().as_deref() == Some("1");
    let qa_log_text = core.qa_log.lock().unwrap().join("\n\n");
    let transcript_text = core.session_transcript.clone();

    match note::save_note(&vault_root, &date, &draft, &transcript_text, &qa_log_text, dry_run) {
        Ok(paths) => {
            if dry_run {
                log::info!("assistant: dry-run, not written: {:?}", paths);
            } else {
                log::info!("assistant: note saved: {:?}", paths);
            }
            core.note_state = NoteState::Saved;
            core.note_error = None;
        }
        Err(e) => {
            core.note_state = NoteState::Failed;
            core.note_error = Some(e);
        }
    }
    core.emit_note();
}

pub async fn discard_note(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    core.note_state = NoteState::Idle;
    core.note_draft = None;
    core.note_markdown.clear();
    core.note_error = None;
    core.emit_note();
}

/// Removes a previously-captured baseline prefix (the open utterance's text
/// as it stood when voice-ask's hotkey was pressed) from freshly-ingested
/// transcript text. A mismatched or empty baseline passes `text` through
/// unchanged, which is what a fresh utterance started after the press
/// should do.
fn strip_voice_baseline(baseline: &str, text: &str) -> String {
    if baseline.is_empty() {
        return text.to_string();
    }
    match text.strip_prefix(baseline) {
        Some(rest) => rest.trim_start().to_string(),
        None => text.to_string(),
    }
}

async fn on_transcript_update(handle: &AssistantHandle, update: TranscriptUpdate) {
    let mut core = handle.0.lock().await;
    if !core.enabled || !core.session_open {
        if core.transcript_ingest_ok {
            core.transcript_ingest_ok = false;
            log::info!(
                "assistant: transcript updates now dropping, enabled={} session_open={}",
                core.enabled, core.session_open
            );
        }
        return;
    }
    if !core.transcript_ingest_ok {
        core.transcript_ingest_ok = true;
        log::info!("assistant: transcript updates now reaching the assistant log");
    }
    let outs = core.transcript.ingest(&update);
    for out in outs {
        match out {
            AssemblerOut::Running { speaker, text } => {
                if speaker == Speaker::You && core.voice.is_capturing() {
                    let voice_text = strip_voice_baseline(&core.voice_baseline, &text);
                    core.voice.note_running(&voice_text);
                }
                core.trigger.consume_running(speaker, &text);
            }
            AssemblerOut::Utterance { speaker, text } => {
                log::info!(
                    "assistant: transcript utterance ingested, speaker={:?} len={}",
                    speaker, text.len()
                );
                if speaker == Speaker::You && core.voice.is_capturing() {
                    let voice_text = strip_voice_baseline(&core.voice_baseline, &text);
                    core.voice.note_utterance(&voice_text);
                    // The utterance the baseline was measured against just
                    // closed; anything after this is a fresh utterance the
                    // baseline should not touch.
                    core.voice_baseline.clear();
                }
                core.trigger.consume_utterance(speaker, &text);
            }
        }
    }
}

/// The claude-turn part of opening a session, isolated so a test can inject
/// a fake `Runner` and check the failure path without a real binary, a real
/// DB, or a running Tauri app.
async fn open_lanes_with(handle: &AssistantHandle, cfg: LaneConfig, seed: String, runner: Runner) {
    let mut core = handle.0.lock().await;
    core.lanes = AnswerLanes::new(runner);
    let emit = core.card_emit.clone();
    core.lanes.open(seed, &cfg, emit).await;
    core.lanes_ready = core.lanes.is_ready();
    core.session_open = true;
    core.last_error = if core.lanes_ready {
        None
    } else {
        Some("assistant lanes failed to open".to_string())
    };
    log::info!(
        "assistant: session opened, session_open=true lanes_ready={}",
        core.lanes_ready
    );
    core.emit_status();
}

/// Bound to `recording-started`. Reloads settings fresh (so a save made
/// before this meeting takes effect), resets the session, and opens both
/// lanes if the assistant is enabled and claude resolves.
///
/// `force_enabled`, when `Some`, wins over the persisted `settings.enabled`
/// for the open/don't-open decision. `set_enabled` passes this when the
/// assistant gets flipped on through the panel switch while a recording is
/// already running: that explicit press should open the session right
/// away, not defer to a possibly-stale settings row that would otherwise
/// leave the session closed until the next stop/start cycle.
async fn open_session(handle: &AssistantHandle, force_enabled: Option<bool>) {
    log::info!("assistant: open_session entered");
    let app = { handle.0.lock().await.app.clone() };

    let settings = match &app {
        Some(app) => match app.try_state::<crate::state::AppState>() {
            Some(state) => AssistantSettings::load(state.db_manager.pool())
                .await
                .unwrap_or_else(|e| {
                    log::warn!("assistant: could not load settings, using defaults: {}", e);
                    AssistantSettings::default()
                }),
            None => AssistantSettings::default(),
        },
        None => AssistantSettings::default(),
    };

    let meeting_name = crate::audio::recording_commands::get_recording_meeting_name()
        .await
        .ok()
        .flatten();

    let (brief, cfg_ready) = {
        let mut core = handle.0.lock().await;
        core.settings = settings.clone();
        core.transcript = TranscriptLog::default();
        core.trigger.reset();
        core.qa_log.lock().unwrap().clear();
        core.note_state = NoteState::Idle;
        core.note_draft = None;
        core.note_markdown.clear();
        core.note_error = None;
        core.session_transcript.clear();
        core.session_open = false;
        core.lanes_ready = false;

        core.mode = parse_mode(&settings.trigger_mode).unwrap_or(TriggerMode::Gated);
        let names = split_names(&settings.names);
        let mode = core.mode;
        let listening = core.listening;
        core.trigger.update(mode, settings.quiet_gap_secs, listening, names);

        if !force_enabled.unwrap_or(settings.enabled) {
            core.enabled = false;
            core.emit_status();
            (None, false)
        } else {
            core.enabled = true;
            (core.pending_brief.take(), true)
        }
    };
    if !cfg_ready {
        log::info!("assistant: open_session stopped, the assistant is disabled in settings");
        return;
    }

    let probe = settings::probe_claude(&settings).await;
    let bin = settings::resolve_claude_binary(&settings).filter(|_| probe.ok);
    {
        let mut core = handle.0.lock().await;
        core.claude_ok = probe.ok;
        if bin.is_none() {
            core.last_error = probe
                .error
                .clone()
                .or_else(|| Some("claude binary not found".to_string()));
            core.emit_status();
            return;
        }
    }
    let bin = bin.expect("checked above");

    let session_dir = std::env::temp_dir()
        .join("meetily-assistant")
        .join(uuid::Uuid::new_v4().to_string());
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        let mut core = handle.0.lock().await;
        core.last_error = Some(format!("could not create the assistant session dir: {}", e));
        core.emit_status();
        return;
    }

    let cfg = LaneConfig {
        cwd: session_dir,
        fast_model: settings.fast_model.clone(),
        fast_effort: settings.fast_effort.clone(),
        deep_model: settings.deep_model.clone(),
        deep_effort: settings.deep_effort.clone(),
        deep_read_dirs: resolve_deep_read_dirs(&settings.deep_read_dirs),
        note_prompt: NOTE_PROMPT.to_string(),
    };

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut seed = format!(
        "Meeting: {}\nDate: {}",
        meeting_name.as_deref().unwrap_or("Untitled meeting"),
        date
    );
    if let Some(brief) = brief.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        seed.push_str("\n\nBrief:\n");
        seed.push_str(brief);
    }

    open_lanes_with(handle, cfg, seed, claude_runner(bin)).await;
}

/// Bound to `recording-stopped`. Leaves the transcript and Q&A log in place
/// for `draft_note`; aborts any turn still in flight.
async fn close_session(handle: &AssistantHandle) {
    let mut core = handle.0.lock().await;
    if !core.session_open {
        return;
    }
    core.session_open = false;
    core.lanes.close();
    core.emit_status();
}

/// Wires the callback fields that need a live `AppHandle` (`trigger.on_fire`,
/// `voice.on_submit`, `voice.publish`), loads settings once for an accurate
/// initial status, and registers the transcript-update / recording-started /
/// recording-stopped listeners. Runs once, from the tauri `setup` hook.
pub fn install(app: AppHandle<Wry>) {
    let init_app = app.clone();
    tauri::async_runtime::spawn(async move {
        wire(init_app).await;
    });
}

async fn wire(app: AppHandle<Wry>) {
    let handle = app.state::<AssistantHandle>().inner().clone();
    let qa_log = { handle.0.lock().await.qa_log.clone() };
    let card_emit = make_card_emit(app.clone(), qa_log);

    {
        let mut core = handle.0.lock().await;
        core.app = Some(app.clone());
        core.card_emit = card_emit;

        let fire_handle = handle.clone();
        core.trigger.on_fire = Arc::new(move |question, _reason| {
            let handle = fire_handle.clone();
            tauri::async_runtime::spawn(async move {
                ask(&handle, question, CardKind::Answer, None).await;
            });
        });

        let submit_handle = handle.clone();
        core.voice.on_submit = Arc::new(move |question: String| {
            let handle = submit_handle.clone();
            tauri::async_runtime::spawn(async move {
                ask(
                    &handle,
                    question,
                    CardKind::Ask,
                    Some("This question was asked by Joseph directly. Do not reply SKIP."),
                )
                .await;
            });
        });
        core.voice.publish = make_voice_publish(app.clone());

        if let Some(state) = app.try_state::<crate::state::AppState>() {
            match AssistantSettings::load(state.db_manager.pool()).await {
                Ok(s) => core.settings = s,
                Err(e) => log::warn!("assistant: could not load settings at startup: {}", e),
            }
        }
        core.claude_ok = settings::probe_claude(&core.settings).await.ok;
        core.mode = parse_mode(&core.settings.trigger_mode).unwrap_or(TriggerMode::Gated);
        let names = split_names(&core.settings.names);
        let mode = core.mode;
        let listening = core.listening;
        let quiet_gap = core.settings.quiet_gap_secs;
        core.trigger.update(mode, quiet_gap, listening, names);
        core.emit_status();
    }

    register_listeners(app, handle);
    log::info!("assistant: listeners registered, wire() complete");
}

fn register_listeners(app: AppHandle<Wry>, handle: AssistantHandle) {
    {
        let handle = handle.clone();
        app.listen("transcript-update", move |event| {
            let Ok(update) = serde_json::from_str::<TranscriptUpdate>(event.payload()) else {
                return;
            };
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                on_transcript_update(&handle, update).await;
            });
        });
    }
    {
        let handle = handle.clone();
        app.listen("recording-started", move |_event| {
            log::info!("assistant: recording-started event received");
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                open_session(&handle, None).await;
            });
        });
    }
    {
        app.listen("recording-stopped", move |_event| {
            log::info!("assistant: recording-stopped event received");
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                close_session(&handle).await;
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::claude_cli::ClaudeOutcome;

    fn tu(source: &str, text: &str, start: f64, end: f64) -> TranscriptUpdate {
        TranscriptUpdate {
            text: text.to_string(),
            timestamp: "00:00:00".to_string(),
            source: source.to_string(),
            sequence_id: 1,
            chunk_start_time: start,
            is_partial: false,
            confidence: 1.0,
            audio_start_time: start,
            audio_end_time: end,
            duration: end - start,
        }
    }

    #[tokio::test]
    async fn set_mode_round_trips_through_get_state() {
        let handle = AssistantHandle::new();
        set_mode(&handle, "continuous").await.unwrap();
        let state = get_state(&handle).await;
        assert_eq!(state.mode, "continuous");
    }

    #[tokio::test]
    async fn set_mode_rejects_an_unknown_mode() {
        let handle = AssistantHandle::new();
        assert!(set_mode(&handle, "sleepwalking").await.is_err());
    }

    #[tokio::test]
    async fn disabled_ignores_transcript_events() {
        let handle = AssistantHandle::new();
        {
            let mut core = handle.0.lock().await;
            core.enabled = false;
            core.session_open = true;
        }

        on_transcript_update(&handle, tu("mic", "hello", 0.0, 1.0)).await;
        // A speaker flip would close "hello" into finals if it had been
        // ingested; while disabled, nothing should have happened at all.
        on_transcript_update(&handle, tu("system", "hi there", 3.0, 4.0)).await;

        let core = handle.0.lock().await;
        let (text, cursor) = core.transcript.delta_since(0);
        assert_eq!(text, "");
        assert_eq!(cursor, 0);
    }

    /// Reproduces the leading-text bleed: speech happens, merges into an
    /// open You utterance, then the hotkey is pressed mid-utterance. What
    /// voice-ask hears must start at the press, not at the utterance's
    /// start.
    #[tokio::test]
    async fn voice_ask_excludes_speech_from_before_the_hotkey() {
        let handle = AssistantHandle::new();
        let published = Arc::new(StdMutex::new(Vec::new()));
        let published_clone = published.clone();
        {
            let mut core = handle.0.lock().await;
            core.enabled = true;
            core.session_open = true;
            core.voice = VoiceAsk::new(
                |_q: String| {},
                move |s: VoiceState| published_clone.lock().unwrap().push(s),
            );
        }

        // Spoken before the hotkey; merges into the open You utterance.
        on_transcript_update(&handle, tu("mic", "Okay.", 0.0, 0.5)).await;

        voice_start(&handle).await;

        // Same utterance continues (same speaker, well inside the 1.2s gap).
        on_transcript_update(&handle, tu("mic", "let's talk about the roadmap", 0.6, 2.0)).await;

        let heard = match published.lock().unwrap().last().cloned() {
            Some(VoiceState::Listening { heard }) => heard,
            other => panic!("expected Listening, got {:?}", other),
        };
        assert_eq!(heard, "let's talk about the roadmap");
    }

    /// A dropped button press must be visible, not silent: the panel only
    /// has `lastError` to show, so pressing Ask, Explain, or Catch up while
    /// closed has to reach it or it just looks like nothing happened.
    #[tokio::test]
    async fn ask_explain_and_catchup_set_last_error_when_session_closed() {
        let handle = AssistantHandle::new();

        ask(&handle, "hello?".to_string(), CardKind::Ask, None).await;
        assert_eq!(
            get_state(&handle).await.last_error.as_deref(),
            Some("Assistant session is not open yet.")
        );

        explain(&handle).await;
        assert_eq!(
            get_state(&handle).await.last_error.as_deref(),
            Some("Assistant session is not open yet.")
        );

        catchup(&handle).await;
        assert_eq!(
            get_state(&handle).await.last_error.as_deref(),
            Some("Assistant session is not open yet.")
        );
    }

    /// Without an active recording, turning the assistant on must not open
    /// a session on its own -- only `recording-started`, or `set_enabled`
    /// catching an already-running recording, should do that.
    #[tokio::test]
    async fn set_enabled_true_does_not_open_a_session_without_a_recording() {
        let handle = AssistantHandle::new();
        set_enabled(&handle, true).await;
        assert!(!get_state(&handle).await.session_open);
    }

    /// A session can be open while the assistant itself is off (toggled off
    /// mid-recording, or disabled by default while a recording that predates
    /// it is still going). explain/catchup must not fire a real Claude call
    /// in that state -- they have to stop at the same `enabled` guard `ask`
    /// already had, not just at `session_open`.
    #[tokio::test]
    async fn explain_and_catchup_set_last_error_when_session_open_but_disabled() {
        let handle = AssistantHandle::new();
        {
            let mut core = handle.0.lock().await;
            core.session_open = true;
            core.enabled = false;
        }

        explain(&handle).await;
        assert_eq!(
            get_state(&handle).await.last_error.as_deref(),
            Some("Assistant is turned off.")
        );

        catchup(&handle).await;
        assert_eq!(
            get_state(&handle).await.last_error.as_deref(),
            Some("Assistant is turned off.")
        );

        // Neither call touched the lanes: still the untouched default,
        // never opened.
        let core = handle.0.lock().await;
        assert!(!core.lanes_ready);
    }

    /// `set_enabled(false)` must leave every piece of state agreeing the
    /// session is closed, not just the lanes. Otherwise `ask`/`explain`/
    /// `catchup` (which check `session_open` first) would sail past their
    /// guard into lanes that were just closed out from under them.
    #[tokio::test]
    async fn set_enabled_false_closes_the_session_too() {
        let handle = AssistantHandle::new();
        {
            let mut core = handle.0.lock().await;
            core.session_open = true;
            core.enabled = true;
        }

        set_enabled(&handle, false).await;

        assert!(!get_state(&handle).await.session_open);
    }

    #[tokio::test]
    async fn lane_open_failure_sets_last_error_and_lanes_ready_false() {
        let handle = AssistantHandle::new();
        let failing_runner: Runner = Arc::new(|_turn, _on_delta, _register| {
            Box::pin(async {
                ClaudeOutcome {
                    session_id: None,
                    text: String::new(),
                    error: Some("boom".to_string()),
                }
            })
        });
        let cfg = LaneConfig {
            cwd: std::env::temp_dir(),
            fast_model: "fast".to_string(),
            fast_effort: "low".to_string(),
            deep_model: "deep".to_string(),
            deep_effort: "medium".to_string(),
            deep_read_dirs: Vec::new(),
            note_prompt: "note".to_string(),
        };

        open_lanes_with(&handle, cfg, "seed".to_string(), failing_runner).await;

        let core = handle.0.lock().await;
        assert!(!core.lanes_ready);
        assert_eq!(core.last_error.as_deref(), Some("assistant lanes failed to open"));
    }
}
