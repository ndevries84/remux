-- Server-hosted Moonfin themes (GET /Moonfin/Themes, admin POST/DELETE
-- /Moonfin/Admin/Themes). Shared by all users.
--
-- json is the raw submitted text, stored verbatim so unknown keys and key
-- order round-trip to the client exactly as uploaded. Moonbase ships no
-- themes, so the table starts empty.
CREATE TABLE moonfin_themes (
    id           TEXT    PRIMARY KEY,
    display_name TEXT    NOT NULL,
    json         TEXT    NOT NULL,
    size_bytes   INTEGER NOT NULL,
    sha256       TEXT    NOT NULL,
    -- epoch milliseconds
    uploaded_at  INTEGER NOT NULL,
    uploaded_by  BLOB    REFERENCES users(id) ON DELETE SET NULL
);
