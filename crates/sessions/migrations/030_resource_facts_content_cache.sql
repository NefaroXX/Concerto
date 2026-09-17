-- ADR-65 §4: bounded content cache columns on `resource_facts` (migration 030).
--
-- Phase 4 read-dedupe: when a plain single-path filesystem read is observed
-- and the row stays clean, the exact bytes that were read are cached here so an
-- identical later read can be served without re-reading the disk — but only
-- after re-validating `size_bytes` + `mtime_ms` against a fresh `stat` AND that
-- `blake3(content_cached) == content_hash`. Either check failing means the
-- cache is not trusted and the tool executes normally (never-stale rule).
--
-- The cache is **derived data** like every other column here: it is never the
-- source of truth, and `rebuild_from_log` deliberately does not repopulate it
-- (a rebuild is a conservative wipe → executing normally until re-observed).
-- Nothing ever reads `content_cached` without the same stat/hash validation
-- the serve path applies.
--
-- Writes happen only through `ResourceFacts::store_read_content` (bounded to
-- `CACHE_LIMIT_BYTES`, NUL-rejecting); the column stays NULL otherwise.
--
-- SQLite ALTER TABLE supports one ADD COLUMN per statement.

ALTER TABLE resource_facts ADD COLUMN content_cached TEXT;
ALTER TABLE resource_facts ADD COLUMN content_cached_bytes INTEGER;