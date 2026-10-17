-- Per-user, per-game emulator choices set from the Moonfin "Choose player"
-- dialog (PUT /Moonfin/Games/{lib}/Games/{id}/Core and .../Backend).
--
-- core is an arcade-family core id (`arcade` or `mame`); backend is `emulatorjs`.
-- Either column may be NULL; a row with both NULL is deleted. game_id is the
-- games index id (a stable v5 UUID over root + relative path) and has no
-- foreign key: games live on disk, and a stale row for a removed game is
-- harmless.
CREATE TABLE moonfin_game_overrides (
    user_id    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    game_id    TEXT    NOT NULL,
    core       TEXT,
    backend    TEXT,
    -- epoch milliseconds
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, game_id)
);
