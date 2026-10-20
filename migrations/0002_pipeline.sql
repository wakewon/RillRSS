-- RillRSS schema v2: pipeline bookkeeping.
--
--   revisions.from_page   1 when the revision's text was extracted from the article page
--                         (set by a successful extract stage and by page updates).
--   revisions.unfinished  runs of the current stage that started but never recorded an
--                         outcome (process crashed / was killed mid-stage).
--   stories.from_page     provenance of the current version and of the change-detection
--                         baseline: 1 = article page, 0 = feed entry.
--   stories.news_versions published versions that count as news (the first one and real
--                         updates; reprocessed versions do not). max_versions and the
--                         version shown in update notices use it; stories.version stays
--                         the internal counter used for item guids.

ALTER TABLE revisions ADD COLUMN from_page INTEGER NOT NULL DEFAULT 0;
ALTER TABLE revisions ADD COLUMN unfinished INTEGER NOT NULL DEFAULT 0;
ALTER TABLE stories ADD COLUMN from_page INTEGER NOT NULL DEFAULT 0;
ALTER TABLE stories ADD COLUMN news_versions INTEGER NOT NULL DEFAULT 0;

UPDATE stories SET news_versions = version;

-- Existing stories of full-text feeds: assume the page unless the revision that produced
-- the current item recorded an extraction fallback.
UPDATE stories SET from_page = 1
 WHERE current_item_id IS NOT NULL
   AND feed_id IN (SELECT id FROM feeds
                    WHERE COALESCE(CASE WHEN json_valid(options) THEN json_extract(options, '$.fulltext') END, 'off') <> 'off')
   AND NOT EXISTS (SELECT 1 FROM items i JOIN revisions r ON r.id = i.revision_id
                    WHERE i.id = stories.current_item_id
                      AND (r.notes LIKE '%全文提取失败%' OR r.notes LIKE '%改用订阅内容%' OR r.notes LIKE '%没有原文链接%'));

-- Dispatcher claim: walk pending revisions of a stage in priority order without touching
-- the (large) rows; next_attempt_at / lease_until are filtered from the index.
CREATE INDEX revisions_claim ON revisions(stage, priority DESC, id, next_attempt_at, lease_until) WHERE state = 'pending';
