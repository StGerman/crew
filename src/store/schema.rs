//! Schema migrations, applied in order at open.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
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
