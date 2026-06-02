-- WEK-26 / M2.3: sessions.import bookkeeping.
--
-- The double-track discovery design (architecture §4.2) lets users
-- promote a backend-native session into the daemon's `sessions` table
-- without copying its on-disk transcript. We persist a pointer to the
-- backend's own file in `external_path` so resume can re-attach the
-- data store, and we make `(backend_id, external_id)` unique so a
-- second import call is idempotent rather than duplicating the row.
--
-- `external_id` was already present (migration 0001) but only carried
-- a non-unique value supplied by older code paths. SQLite's `UNIQUE`
-- index treats multiple NULLs as distinct, so native (origin='user')
-- rows that leave `external_id` NULL keep coexisting without contest.

ALTER TABLE sessions
    ADD COLUMN external_path TEXT;

-- Idempotent import: same (backend, external_id) ⇒ same row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_backend_external_id
    ON sessions(backend_id, external_id)
    WHERE external_id IS NOT NULL;

INSERT OR REPLACE INTO schema_meta(key, value) VALUES
  ('schema_version', '3'),
  ('migration', '0003_session_external_reference');
