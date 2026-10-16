-- Cache of unfiltered MDBList ratings for the Moonfin ratings proxy.
--
-- Persisted (rather than memory-only) because MDBList's free tier allows
-- only 1,000 requests a day and a restart must not re-spend that quota.
CREATE TABLE mdblist_ratings_cache (
    -- "movie:<tmdb id>" or "show:<tmdb id>"
    cache_key  TEXT    NOT NULL PRIMARY KEY,
    -- JSON array of {source, value, score, votes, url}
    ratings    TEXT    NOT NULL,
    -- epoch seconds
    fetched_at INTEGER NOT NULL
);
CREATE INDEX mdblist_ratings_cache_fetched_at ON mdblist_ratings_cache (fetched_at);
