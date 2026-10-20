-- RillRSS schema v1
--
-- Concepts
--   feed      : a subscription.
--   story     : one news article inside a feed, across all of its revisions.
--               Identified by any of its keys (normalized guid / canonical url).
--   revision  : one snapshot of a story's source going through the processing
--               pipeline (extract -> transform -> translate -> images -> publish).
--               Revisions are the persistent work queue.
--   item      : a published, client-visible article. items.id is the public
--               article id handed to readers. It is allocated when the item is
--               published (AUTOINCREMENT => monotonic, never reused), so
--               incremental sync by since_id / newer-than works even when
--               processing finishes out of order.

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE sessions (
    token_hash   TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,            -- web | greader | ttrss
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    expires_at   INTEGER,                  -- NULL = no expiry
    user_agent   TEXT,
    remote_addr  TEXT
) WITHOUT ROWID;

CREATE TABLE categories (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE feeds (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    url               TEXT NOT NULL UNIQUE,
    title             TEXT NOT NULL DEFAULT '',
    title_locked      INTEGER NOT NULL DEFAULT 0,
    site_url          TEXT,
    description       TEXT,
    category_id       INTEGER REFERENCES categories(id) ON DELETE SET NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    fetch_interval    INTEGER,                 -- minutes, NULL = global default
    options           TEXT NOT NULL DEFAULT '{}',  -- JSON FeedOptions
    etag              TEXT,
    last_modified     TEXT,
    last_fetch_at     INTEGER,
    last_success_at   INTEGER,
    next_fetch_at     INTEGER NOT NULL DEFAULT 0,
    fetch_error_count INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    last_status       INTEGER,
    last_duration_ms  INTEGER,
    lease_until       INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX feeds_next_fetch ON feeds(enabled, next_fetch_at);
CREATE INDEX feeds_category ON feeds(category_id);

CREATE TABLE feed_icons (
    feed_id    INTEGER PRIMARY KEY REFERENCES feeds(id) ON DELETE CASCADE,
    mime       TEXT NOT NULL,
    data       BLOB NOT NULL,
    hash       TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);

CREATE TABLE stories (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_id           INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    guid              TEXT NOT NULL,           -- latest raw guid
    link              TEXT,
    title             TEXT NOT NULL DEFAULT '',
    published_at      INTEGER,                 -- original publication time (secs)
    first_seen_at     INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    entry_hash        TEXT,                    -- fingerprint of the last raw feed entry
    source_title      TEXT,                    -- title of the last accepted source
    source_text       TEXT,                    -- normalized text of the last accepted source
    version           INTEGER NOT NULL DEFAULT 0,  -- number of published versions
    current_item_id   INTEGER,
    last_published_at INTEGER,
    next_check_at     INTEGER,                 -- next original-page update check
    check_count       INTEGER NOT NULL DEFAULT 0,
    state             TEXT NOT NULL DEFAULT 'active'  -- active | skipped | frozen
);
CREATE INDEX stories_feed ON stories(feed_id, last_seen_at);
CREATE INDEX stories_check ON stories(next_check_at) WHERE next_check_at IS NOT NULL;

-- Every identity key ever observed for a story: 'g:<guid key>' or 'u:<url key>'.
CREATE TABLE story_keys (
    feed_id  INTEGER NOT NULL,
    key      TEXT NOT NULL,
    story_id INTEGER NOT NULL REFERENCES stories(id) ON DELETE CASCADE,
    PRIMARY KEY (feed_id, key)
) WITHOUT ROWID;
CREATE INDEX story_keys_story ON story_keys(story_id);

CREATE TABLE revisions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    story_id         INTEGER NOT NULL REFERENCES stories(id) ON DELETE CASCADE,
    feed_id          INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    reason           TEXT NOT NULL,            -- new | entry_update | page_update | reprocess
    state            TEXT NOT NULL,            -- pending | published | unchanged | failed | obsolete
    stage            TEXT NOT NULL,            -- extract | transform | translate | images | publish | done
    priority         INTEGER NOT NULL DEFAULT 0,
    guid             TEXT NOT NULL,
    title            TEXT NOT NULL,
    link             TEXT,
    author           TEXT,
    published_at     INTEGER,
    entry_updated_at INTEGER,
    entry_html       TEXT,                     -- content or summary from the feed
    media            TEXT,                     -- JSON media (enclosures, thumbnail)
    content_html     TEXT,                     -- working HTML (output of the last finished stage)
    title_out        TEXT,                     -- working title
    source_text      TEXT,                     -- normalized source text (for change detection)
    lang             TEXT,
    notes            TEXT NOT NULL DEFAULT '[]',  -- JSON array of warnings
    attempts         INTEGER NOT NULL DEFAULT 0,  -- attempts in the current stage
    total_attempts   INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  INTEGER NOT NULL DEFAULT 0,
    lease_until      INTEGER,
    last_error       TEXT,
    item_id          INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE INDEX revisions_due ON revisions(stage, next_attempt_at) WHERE state = 'pending';
CREATE INDEX revisions_story ON revisions(story_id, state);
CREATE INDEX revisions_state ON revisions(state, updated_at);

CREATE TABLE items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,  -- public article id
    feed_id       INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    story_id      INTEGER REFERENCES stories(id) ON DELETE SET NULL,
    revision_id   INTEGER,
    version       INTEGER NOT NULL DEFAULT 1,
    guid          TEXT NOT NULL,
    link          TEXT,
    title         TEXT NOT NULL,
    author        TEXT,
    content       TEXT NOT NULL,
    excerpt       TEXT NOT NULL DEFAULT '',
    media         TEXT,
    lang          TEXT,
    published_at  INTEGER NOT NULL,      -- seconds; article date shown to readers
    updated_at    INTEGER NOT NULL,      -- seconds
    created_at    INTEGER NOT NULL,      -- microseconds; when it became visible (crawl time)
    is_read       INTEGER NOT NULL DEFAULT 0,
    is_starred    INTEGER NOT NULL DEFAULT 0,
    is_published  INTEGER NOT NULL DEFAULT 0,
    read_at       INTEGER,
    starred_at    INTEGER,
    note          TEXT,
    superseded_by INTEGER,
    hidden        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX items_feed ON items(feed_id, hidden, id);
CREATE INDEX items_unread ON items(hidden, is_read, id);
CREATE INDEX items_starred ON items(is_starred, id) WHERE is_starred = 1;
CREATE INDEX items_created ON items(created_at);
CREATE INDEX items_published ON items(published_at);
CREATE INDEX items_story ON items(story_id);

CREATE TABLE labels (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    created_at INTEGER NOT NULL
);

CREATE TABLE item_labels (
    item_id  INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, label_id)
) WITHOUT ROWID;
CREATE INDEX item_labels_label ON item_labels(label_id);

-- Segment-level translation cache.
CREATE TABLE translations (
    key        TEXT PRIMARY KEY,   -- blake3(model, target, prompt version, source)
    target     TEXT NOT NULL,
    model      TEXT NOT NULL,
    source     TEXT NOT NULL,
    result     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    used_at    INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX translations_used ON translations(used_at);

-- Content-addressed image cache.
CREATE TABLE images (
    hash       TEXT PRIMARY KEY,   -- blake3 of the bytes
    mime       TEXT NOT NULL,
    ext        TEXT NOT NULL,
    size       INTEGER NOT NULL,
    width      INTEGER,
    height     INTEGER,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE image_sources (
    url        TEXT PRIMARY KEY,
    hash       TEXT,               -- NULL when the download failed / was skipped
    status     TEXT NOT NULL,      -- ok | failed | skipped
    error      TEXT,
    attempts   INTEGER NOT NULL DEFAULT 0,
    fetched_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX image_sources_hash ON image_sources(hash);

CREATE TABLE item_images (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    hash    TEXT NOT NULL,
    PRIMARY KEY (item_id, hash)
) WITHOUT ROWID;
CREATE INDEX item_images_hash ON item_images(hash);

-- Diagnostic event log (bounded; pruned by maintenance).
CREATE TABLE events (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts       INTEGER NOT NULL,
    level    TEXT NOT NULL,        -- info | warn | error
    kind     TEXT NOT NULL,        -- fetch | extract | translate | images | publish | update | cleanup | system | api
    feed_id  INTEGER,
    story_id INTEGER,
    item_id  INTEGER,
    message  TEXT NOT NULL
);
CREATE INDEX events_ts ON events(ts);
CREATE INDEX events_feed ON events(feed_id, ts);
