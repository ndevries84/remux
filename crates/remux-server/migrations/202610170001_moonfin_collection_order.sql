-- Per-user custom ordering of a collection's items (Moonfin drag-reorder).
--
-- The client stores the whole flat order (movie and episode ids, including
-- ids that are not direct collection members), so this is an opaque ordered
-- list per (user, collection) rather than a native collection weight.
-- collection_id has no foreign key: Moonbase accepts an order for any id, and
-- stale rows for deleted collections are harmless.
CREATE TABLE moonfin_collection_order (
    user_id       BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    collection_id BLOB    NOT NULL,
    -- JSON array of item id strings, exactly as the client sent them
    item_ids      TEXT    NOT NULL,
    -- epoch milliseconds
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, collection_id)
);
