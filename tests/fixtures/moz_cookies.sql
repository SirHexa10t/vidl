-- The real Firefox cookie-DB shape (the moz_cookies table, current column set) — realistic
-- enough that both vidl's import filter and yt-dlp's own firefox cookie loader read a fixture
-- built from it exactly like a genuine profile DB. Used by tests/cookie_import.rs.
CREATE TABLE moz_cookies (
    id INTEGER PRIMARY KEY,
    originAttributes TEXT NOT NULL DEFAULT '',
    name TEXT,
    value TEXT,
    host TEXT,
    path TEXT,
    expiry INTEGER,
    lastAccessed INTEGER,
    creationTime INTEGER,
    isSecure INTEGER,
    isHttpOnly INTEGER,
    inBrowserElement INTEGER DEFAULT 0,
    sameSite INTEGER DEFAULT 0,
    rawSameSite INTEGER DEFAULT 0,
    schemeMap INTEGER DEFAULT 0
);
PRAGMA user_version = 12;
