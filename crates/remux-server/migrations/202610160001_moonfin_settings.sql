-- Per-user, per-profile Moonfin client settings (settings sync).
--
-- One row per (user, profile) holding a sparse JSON object. The profile
-- object is deliberately schemaless: the client pushes ~100 keys and new
-- releases add more, so unknown keys must round-trip untouched.
CREATE TABLE moonfin_settings (
    user_id    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- global | desktop | mobile | tv (always lowercase)
    profile    TEXT    NOT NULL CHECK (profile IN ('global', 'desktop', 'mobile', 'tv')),
    data       TEXT    NOT NULL,
    -- epoch milliseconds
    updated_at INTEGER NOT NULL,
    -- clientId of the last writer
    updated_by TEXT,
    PRIMARY KEY (user_id, profile)
);
