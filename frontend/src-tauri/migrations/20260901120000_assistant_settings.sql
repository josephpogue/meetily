-- Settings for the live meeting assistant (assistant/ module). Single row, id='1'.
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
