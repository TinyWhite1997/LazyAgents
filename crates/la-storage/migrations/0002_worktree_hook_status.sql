-- WEK-27 / M2.4: per-session git worktree state.
--
-- M1 already shipped the worktree_path / worktree_branch / base_branch
-- columns on `sessions` (migration 0001). M2.4 needs one more column to
-- record the outcome of the optional .lazyagents/hooks/post-create.sh
-- script that runs once after `git worktree add` succeeds.
--
-- The column is a soft signal — hook failure does NOT roll back the
-- worktree, abort the spawn, or change `SessionStatus`. Per the M2 brief
-- amendment R4, it is rendered as a separate badge in the TUI; the
-- enum / state path is unchanged. NULL on rows created before this
-- migration distinguishes "we don't know" from "explicitly skipped".

ALTER TABLE sessions
    ADD COLUMN post_create_hook_status TEXT
    CHECK (post_create_hook_status IN ('ok', 'failed', 'skipped', 'timeout'));

-- TTL sweep: WorktreeManager::sweep_expired scans archived rows older
-- than `now - ttl` so the index keeps that path off a full table scan
-- once `sessions` grows past a few hundred rows.
CREATE INDEX IF NOT EXISTS idx_sessions_archived_at
    ON sessions(archived_at)
    WHERE archived_at IS NOT NULL;

-- prune_orphans + diff panel both want "which sessions still own a
-- worktree under this project" — a partial index keeps the lookup tight
-- because the live working-tree set is always a small fraction of all
-- session rows.
CREATE INDEX IF NOT EXISTS idx_sessions_project_worktree
    ON sessions(project_id)
    WHERE worktree_path IS NOT NULL;

INSERT OR REPLACE INTO schema_meta(key, value) VALUES
  ('schema_version', '2'),
  ('migration', '0002_worktree_hook_status');
