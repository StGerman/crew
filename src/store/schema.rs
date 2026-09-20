//! Schema migrations, applied in order at open.

use rusqlite::Connection;

pub(super) const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE IF NOT EXISTS issue_state (
      issue_id          TEXT PRIMARY KEY,
      identifier        TEXT NOT NULL,
      worktree_key      TEXT NOT NULL,
      phase             TEXT NOT NULL DEFAULT 'released',
      attempt           INTEGER NOT NULL DEFAULT 0,
      consecutive_fail  INTEGER NOT NULL DEFAULT 0,
      last_fail_class   TEXT,
      cumulative_turns  INTEGER NOT NULL DEFAULT 0,
      miss_count        INTEGER NOT NULL DEFAULT 0,
      -- Tracker state at the moment a run reported Done or Blocked. While the issue still sits
      -- in that state there is nothing new to act on, so it is not re-dispatched. Cleared as
      -- soon as the state moves.
      parked_state      TEXT,
      quarantined_at    INTEGER,
      last_error_class  TEXT,
      last_error        TEXT,
      task_ref          TEXT,
      updated_at        INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS run (
      run_id     TEXT PRIMARY KEY,
      issue_id   TEXT NOT NULL REFERENCES issue_state(issue_id),
      started_at INTEGER NOT NULL,
      ended_at   INTEGER,
      outcome    TEXT,
      session_id TEXT,
      turns      INTEGER NOT NULL DEFAULT 0,
      in_tok     INTEGER NOT NULL DEFAULT 0,
      out_tok    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS run_by_issue ON run(issue_id, started_at DESC);

    CREATE TABLE IF NOT EXISTS retry (
      issue_id TEXT PRIMARY KEY REFERENCES issue_state(issue_id),
      due_at   INTEGER NOT NULL,
      attempt  INTEGER NOT NULL,
      reason   TEXT
    );
    "#,
    // v2
    r#"
    -- The conversation an issue's work is happening in. A continuation resumes it instead of
    -- starting cold. Nullable in both directions that matter: an issue that has never been
    -- dispatched has no session yet, and a run that proves its session unresumable clears the
    -- column rather than retrying into the same dead id.
    ALTER TABLE issue_state ADD COLUMN session_id TEXT;
    "#,
    // v3
    r#"
    -- Token totals become nullable, and every total recorded so far is dropped rather than
    -- carried over. Until this version they were sums of per-event stream usage, which
    -- double-counts input by the turn count and undercounts output by orders of magnitude
    -- (issue #9). Those figures are not a rough version of the truth; they are unrelated to it,
    -- and a NULL is the honest replacement. From here on NULL means "this run ended without the
    -- CLI reporting a total" — killed, crashed, or cut off by the turn budget — and the sums the
    -- dashboard shows cover only the runs that did report. SQLite cannot drop a NOT NULL, so the
    -- table is rebuilt; nothing references `run`, so the rebuild has no cascade to worry about.
    CREATE TABLE run_v3 (
      run_id     TEXT PRIMARY KEY,
      issue_id   TEXT NOT NULL REFERENCES issue_state(issue_id),
      started_at INTEGER NOT NULL,
      ended_at   INTEGER,
      outcome    TEXT,
      session_id TEXT,
      turns      INTEGER NOT NULL DEFAULT 0,
      in_tok     INTEGER,
      out_tok    INTEGER
    );
    INSERT INTO run_v3 (run_id, issue_id, started_at, ended_at, outcome, session_id, turns)
      SELECT run_id, issue_id, started_at, ended_at, outcome, session_id, turns FROM run;
    DROP TABLE run;
    ALTER TABLE run_v3 RENAME TO run;
    CREATE INDEX IF NOT EXISTS run_by_issue ON run(issue_id, started_at DESC);
    "#,
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", version)?;
        }
    }
    Ok(())
}
