# Meetily Live Assistant (meeting-assistant v2) - Design

Fork of meetily v0.4.0 (Tauri 2 Rust core + Next.js UI) that adds a live, in-meeting Claude assistant: it reads the live transcript as the meeting happens, answers questions on demand (typed or spoken), volunteers answers when the room asks Joseph something, explains and catches up on demand, and drafts a meeting note at the end. This is version 2 of the meeting-assistant product. The v1 Swift app stays as-is; the previously sketched diarization rework becomes the v3 backlog.

Upstream posture: everything new lives in an `assistant/` Rust module and an `AssistantPanel` UI component. Edits to upstream files are minimal and enumerated below, so pulling future meetily releases stays cheap.

## Binding constraints (carried from v1)

- Subscription only: every model call is a local `claude -p` spawn. No paid APIs, no API keys, and the existing meetily summary providers are left untouched.
- All audio and transcripts stay on-machine. The only external path is Anthropic via the claude CLI.
- The assistant contextualizes and converses; it never acts. Nothing is written anywhere without an explicit Save press.
- Output is text only. No TTS.
- macOS, Apple Silicon.

Dropped from v1 (deliberate): the screen-share-invisible overlay (meetily is a normal window; do not describe the app as hidden). Deferred to later (Joseph's call): the pre-meeting Brief, the Resolver, and the Context Pack; the trunk seed takes an optional plain typed brief instead, unresolved.

## Architecture

Five new pieces, one small upstream edit.

1. `frontend/src-tauri/src/assistant/` (new Rust module): claude CLI runner, answer lanes, trigger engine, voice ask, note drafter, Tauri commands. Owns all assistant state in a managed `AssistantState`.
2. `frontend/src/components/AssistantPanel/` (new UI): the panel next to the transcript.
3. Settings: a new `assistant_settings` SQLite table (own migration) plus an Assistant tab in Settings.
4. Source tagging (the one behavioral upstream edit): `audio/pipeline.rs` tags each VAD segment `mic` or `system` before the mixer erases identity.
5. Registration touches: `lib.rs` (mod + manage + generate_handler entries), `app/page.tsx` (mount panel), `settings/page.tsx` (tab), `types/index.ts` (types).

Transcript feed: the assistant listens to the existing `transcript-update` Tauri event from Rust (`app.listen`), so the transcription worker is not modified. Assistant failures must never touch the recording or transcription path; the module is fully isolated and every entry point catches its own errors.

Lifecycle: when recording starts (existing `recording-started` event) and the assistant is enabled, the assistant session opens (trunk seed + lane forks). When recording stops, lanes close and the note flow becomes available. The panel also has explicit enable/disable.

## Source tagging (You/Them without diarization)

Today `AudioPipeline::run` mixes mic and system windows, then cuts VAD segments from the mix; `TranscriptUpdate.source` is hardcoded `"Audio"`. The fix: before `mix_window`, compare RMS energy of the mic window and the system window; tag the resulting segment `"mic"` if mic dominates, `"system"` otherwise. Thread the tag through the existing `AudioChunk.device_type` into the worker so `TranscriptUpdate.source` carries `"mic"`/`"system"` (keep `"Audio"` only where a mix is genuinely ambiguous). Check every frontend read of `source` before changing the value set.

The assistant maps mic to `You:` and system to `Them:` in prompts and the transcript log. This is channel identity, not diarization; who among Them is speaking stays out of scope. RMS dominance is approximate; the fast lane self-gates with SKIP, so a mistagged segment costs a spawn, not a wrong card.

## Claude runner (`assistant/claude_cli.rs`)

Port of v1's ClaudeCLI.swift onto `tokio::process::Command`. One spawn per turn:

`claude -p <prompt> --output-format stream-json --verbose --include-partial-messages [--safe-mode] [--resume <id>] [--fork-session] [--session-id <id>] [--model m] [--effort e] [--append-system-prompt s] [--allowedTools ...] [--disallowedTools ...] [--add-dir ...]`

- Parse stdout as NDJSON: `system/init` yields the session id; `stream_event` content_block_delta text_delta yields live tokens; `result` yields final text and error flag. Keep a stderr tail for error messages.
- `--safe-mode` on every lane turn (drops CLAUDE.md stack and output styles; measured in v1 it cuts the cached prefix ~6x and protects the card format).
- Binary resolution: `assistant_settings.claude_path` if set, else `which claude`, else common install paths. Never bundled; never reuse the llama-helper sidecar (wrong lifecycle: that one idles out and is name-hardcoded).
- Hold child handles so `assistant_stop` can terminate in-flight turns.

## Answer lanes (`assistant/lanes.rs`)

Port of v1's AnswerLanes, decisions unchanged:

- On session open: seed one trunk session (`--session-id`) with the meeting header (title, date) plus the optional typed brief; then `--fork-session` twice: fast lane `claude-sonnet-5 --effort low`, deep lane `claude-opus-5 --effort medium`. Effort pinned per lane for the whole meeting.
- Every turn is `claude -p --resume <lane> `; the prompt is the transcript delta since that lane's last turn, never a resend. Per-lane cursors over the assistant's transcript log.
- No queueing: a trigger landing while a lane turn is in flight runs as `--fork-session` of that lineage, seeded with the delta; the fork does not advance the cursor.
- Fast pass: no tools, answers from seed + transcript, streams into the card, may return SKIP (renders nothing). Hold the first tokens until SKIP is ruled out.
- Deep pass: fires on the same trigger, read-only tools (Read, Grep, Glob) with `--add-dir ~/brain`, re-derives the answer, settles the card. Contradiction detection compares fast and deep cards; a correction marks the card and highlights changed lines.
- Explain and Catch-up are fast-lane only and land already checked. Explain covers the last ~15 s of Them; the first Catch-up covers the meeting so far, later ones the last 5 minutes.
- Note drafting is one turn on a fork of the deep lineage with the note prompt.
- Prompts: copy v1's `prompts/fast.md`, `deep.md`, `note.md` into `assistant/prompts/` (embedded via `include_str!`), with two edits: the Context Pack wording softens to "the brief you were given, if any", and the note prompt drops the LINE section (no linked project pages in v2).

## Utterances and triggers (`assistant/trigger.rs`)

Meetily's VAD already cuts speech into segments, so utterance assembly is lighter than v1: consecutive same-source segments merge into one utterance until a quiet gap (~1.2 s) or a source flip closes it. Partial updates (`is_partial`) update a running buffer for the name-call early path and voice ask.

Trigger engine is a direct port of v1's TriggerEngine, working on Them utterances only:

- Modes, cycled live: Manual (never volunteers), Gated (default: name-call fires at once, other Them questions after a 2.0 s room-quiet gap), Continuous (any Them question).
- Local tests only, no model: substantive (at least 2 real words), looks-like-a-question (mark or interrogative shape), mentions-Joseph (name list from settings), repeat guard (0.7 content-word overlap against the last 5 fires).
- The engine proposes; the fast lane disposes (answer or SKIP).

## Voice ask (`assistant/voice_ask.rs`)

Port of v1's VoiceAsk. Nothing new listens: the mic is already transcribed. A voice ask brackets mic-tagged text between start and stop: start on hotkey or button, live "heard" text mirrors to the panel, auto-submit after 1.5 s of silence, 45 s cap, Escape cancels, second press submits. The submitted text goes through the same path as a typed ask.

Known risk: v1 rode 1-1.5 s streaming drafts; meetily emits per-VAD-segment results and the partial-emission cadence is unverified. Measure it early; if partials are too sparse the heard-text mirror updates per segment, which is acceptable but less lively.

## Assistant panel (frontend)

Sibling of `TranscriptPanel` inside the existing flex row on `app/page.tsx`; Tailwind + the repo's shadcn primitives; collapsible to a slim rail toggle. TranscriptPanel's width constraint relaxes to share the row.

- Header: listening dot (click pauses triggers), mode chip (click cycles), assistant on/off.
- Card stream: current card on top, history below with timestamps. Card = lead line, up to 3 bullets, small mono source line. Phases: drafting (amber accent, streams in), checked (neutral), corrected (visible tag, changed lines briefly highlighted). One card per trigger; deep settles it in place.
- Ask box: typed question, Enter submits; voice button beside it showing capture state and live heard text.
- Explain and Catch-up buttons.
- End of meeting: a note bar appears (Draft note -> preview -> Save / Discard). Preview renders the markdown; Save is the only write.
- Empty states for: assistant off, claude binary missing (points at Settings), lanes warming, no cards yet.

Hotkeys via `tauri-plugin-global-shortcut` (add the plugin): Option-A voice ask, Option-E explain, Option-C catch-up, Option-M cycle mode. Rebindable later; in-window key handling as fallback if plugin registration fails.

## Notes (`assistant/note.rs`)

On demand after recording stops: one deep-lineage fork turn with the note prompt over the full labeled transcript. Preview in the panel. Save writes two files under `~/brain/wiki/_sources/meetings/`: `YYYY-MM-DD-<slug>.md` (summary, decisions, action items, open questions, Q&A log from the card history) and `YYYY-MM-DD-<slug>_transcript.md` (full You/Them transcript), then commits both in the vault repo. Vault root from settings, `--dry-run` env override for tests. Meetily's own SQLite meeting save is untouched and still happens.

## Settings

New migration `assistant_settings` (single row, id='1'): `enabled`, `claude_path`, `fast_model`, `fast_effort`, `deep_model`, `deep_effort`, `trigger_mode`, `quiet_gap_secs`, `names` (comma list, default "joseph,joe"), `vault_root`, `deep_read_dirs`. Defaults as above. New Settings tab "Assistant" modeled on SummaryModelSettings, plus a "Test claude" button that runs a one-token spawn and reports auth state. The name "Assistant" avoids the existing "claude"/anthropic summary-provider namespace.

## Commands and events (the frontend/backend contract, pinned)

Commands (all return `Result<T, String>`):
`assistant_get_state`, `assistant_set_enabled(bool)`, `assistant_ask(text)`, `assistant_explain`, `assistant_catchup`, `assistant_set_mode(mode)`, `assistant_set_listening(bool)`, `assistant_voice_start`, `assistant_voice_finish`, `assistant_voice_cancel`, `assistant_draft_note`, `assistant_save_note`, `assistant_discard_note`, `assistant_get_settings`, `assistant_save_settings(settings)`, `assistant_test_claude`.

Events:
- `assistant-card`: full card upsert `{ id, kind: "answer"|"ask"|"explain"|"catchup", question, lead, bullets: string[], source, phase: "drafting"|"checked"|"corrected", changed_lines: string[], ts }`.
- `assistant-status`: `{ enabled, session_open, lanes_ready, mode, listening, claude_ok, last_error }`.
- `assistant-voice`: `{ state: "off"|"listening"|"submitting", heard }`.
- `assistant-note`: `{ state: "idle"|"drafting"|"ready"|"saved"|"failed", markdown, error }`.

## Error handling

- A failed lane turn updates `assistant-status.last_error` and, if a card was open, settles it as checked-with-fast-content rather than leaving amber forever.
- Missing or unauthenticated claude binary: `claude_ok=false`, panel shows the fix, nothing spawns.
- Recording and transcription never wait on, and never fail because of, the assistant.

## Testing

- Unit: trigger engine (port v1's cases: fragments, repeats, name-call arming, gap restarts), utterance merge, card parse (LEAD/bullets/SOURCE/SKIP), RMS tagger.
- Integration (env-gated, real subscription): one fast-lane turn end to end through the runner.
- End to end, as a user hits it (the guard that counts): dev build; start a recording; play a spoken WAV aloud so mic and system capture hear it; watch the transcript flow; typed ask streams a card that settles; Explain and Catch-up render; stop; draft note preview appears; dry-run save. Screenshot the panel states.

## Out of scope for v2

Speaker diarization. Brief/Resolver/Context Pack. Screen-share invisibility. Follow-up email drafts, TODO pushes, calendar. Upstreaming the source tag (worth a PR to meetily later, separately).
