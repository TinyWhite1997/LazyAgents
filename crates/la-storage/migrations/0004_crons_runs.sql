-- WEK-34 / M3.3: cron definitions and per-trigger run history.
--
-- The scheduler owns in-memory timing, but these tables are the durable
-- source for cron definitions, catch-up watermarks, and run history.

CREATE TABLE crons (
  id                            TEXT PRIMARY KEY,
  name                          TEXT NOT NULL,
  enabled                       INTEGER NOT NULL DEFAULT 0,
  project_id                    TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  backend_id                    TEXT NOT NULL REFERENCES backends(id),
  spawn_args                    TEXT NOT NULL,
  prompt                        TEXT NOT NULL,
  cron_expr                     TEXT NOT NULL,
  tz                            TEXT NOT NULL,
  catchup_mode                  TEXT NOT NULL DEFAULT 'coalesce'
                                  CHECK (catchup_mode IN ('skip', 'coalesce', 'replay')),
  max_concurrent_runs           INTEGER NOT NULL DEFAULT 1 CHECK (max_concurrent_runs >= 1),
  max_runs_per_day              INTEGER NOT NULL DEFAULT 24 CHECK (max_runs_per_day >= 1),
  max_runtime_s                 INTEGER NOT NULL DEFAULT 1800 CHECK (max_runtime_s >= 1),
  cost_budget_usd_per_day       REAL,
  failure_backoff               TEXT NOT NULL DEFAULT 'expo(1m,2,1h)',
  pause_on_consecutive_failures INTEGER NOT NULL DEFAULT 5 CHECK (pause_on_consecutive_failures >= 1),
  consecutive_failures          INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
  last_fired_at                 TEXT,
  next_fire_at                  TEXT,
  created_at                    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at                    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_crons_project ON crons(project_id);
CREATE INDEX idx_crons_backend ON crons(backend_id);
CREATE INDEX idx_crons_next ON crons(next_fire_at) WHERE enabled = 1;

CREATE TABLE runs (
  id              TEXT PRIMARY KEY,
  cron_id         TEXT REFERENCES crons(id) ON DELETE SET NULL,
  session_id      TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  scheduled_at    TEXT NOT NULL,
  started_at      TEXT,
  finished_at     TEXT,
  status          TEXT NOT NULL CHECK (
                    status IN (
                      'pending',
                      'spawning',
                      'running',
                      'completed',
                      'failed',
                      'timed_out',
                      'cancelled',
                      'budget_exceeded'
                    )
                  ),
  exit_code       INTEGER,
  coalesced_count INTEGER NOT NULL DEFAULT 1 CHECK (coalesced_count >= 1),
  cost_usd_est    REAL,
  error_kind      TEXT,
  error_detail    TEXT,
  tail_log        BLOB
);

CREATE INDEX idx_runs_cron_time ON runs(cron_id, scheduled_at DESC);
CREATE INDEX idx_runs_status ON runs(status);
CREATE INDEX idx_runs_scheduled_at ON runs(scheduled_at);
CREATE INDEX idx_runs_session ON runs(session_id) WHERE session_id IS NOT NULL;

INSERT OR REPLACE INTO schema_meta(key, value) VALUES
  ('schema_version', '4'),
  ('migration', '0004_crons_runs');
