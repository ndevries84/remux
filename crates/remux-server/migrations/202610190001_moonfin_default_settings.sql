-- Admin default Moonfin settings: the lowest layer when a user's settings
-- are resolved (device, global, defaults). Edited through the admin API
-- (GET|PUT /Moonfin/Admin/Defaults).
--
-- A single row (id = 1) holding a schemaless JSON object, like a user
-- profile. No row means no defaults were ever saved, which is what lets the
-- optional one-time import of `moonfin_default_settings_path` run exactly
-- once.
CREATE TABLE moonfin_default_settings (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    data       TEXT    NOT NULL,
    -- epoch milliseconds
    updated_at INTEGER NOT NULL,
    updated_by BLOB    REFERENCES users(id) ON DELETE SET NULL
);
