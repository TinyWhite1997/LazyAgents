PRAGMA foreign_keys = ON;

CREATE TABLE backends (
  id              TEXT PRIMARY KEY,
  display_name    TEXT NOT NULL,
  version         TEXT,
  available       INTEGER NOT NULL DEFAULT 0,
  last_probed_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE projects (
  id              TEXT PRIMARY KEY,
  root_path       TEXT NOT NULL UNIQUE,
  display_name    TEXT NOT NULL,
  vcs             TEXT,
  created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_projects_root ON projects(root_path);

CREATE TABLE sessions (
  id               TEXT PRIMARY KEY,
  project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
  backend_id       TEXT NOT NULL REFERENCES backends(id),
  external_id      TEXT,
  title            TEXT,
  state            TEXT NOT NULL,
  exit_code        INTEGER,
  pid              INTEGER,
  worktree_path    TEXT,
  worktree_branch  TEXT,
  base_branch      TEXT,
  spawn_args       TEXT NOT NULL,
  origin           TEXT NOT NULL DEFAULT 'user',
  transcript_path  TEXT,
  transcript_bytes INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
  archived_at      TEXT
);
CREATE INDEX idx_sessions_project ON sessions(project_id) WHERE archived_at IS NULL;
CREATE INDEX idx_sessions_state ON sessions(state);
CREATE INDEX idx_sessions_origin ON sessions(origin);

CREATE TABLE session_chunks (
  session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq             INTEGER NOT NULL,
  ts              TEXT NOT NULL DEFAULT (datetime('now')),
  kind            TEXT NOT NULL,
  data            BLOB NOT NULL,
  PRIMARY KEY(session_id, seq)
) WITHOUT ROWID;
CREATE INDEX idx_session_chunks_ts ON session_chunks(session_id, ts);

CREATE TABLE settings (
  key             TEXT PRIMARY KEY,
  value           TEXT NOT NULL
);

CREATE TABLE schema_meta (
  key             TEXT PRIMARY KEY,
  value           TEXT NOT NULL
);

INSERT INTO schema_meta(key, value) VALUES
  ('schema_version', '1'),
  ('migration', '0001_initial');
