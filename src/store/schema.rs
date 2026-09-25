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
    // v4
    r#"
    -- The branch a dispatched run actually checked out, recorded once at `prepare` time rather
    -- than recomputed from `identifier` on every read. A recomputed name can drift two ways:
    -- `Store::ensure` may rename `identifier` after launch, changing what the recompute would
    -- produce, and `Workspace::remove` deletes the branch whenever git's merged check says it
    -- holds nothing new, so a name that still resolves says nothing about whether the ref still
    -- exists. NULL means what it means everywhere else in this table: nothing to report, either
    -- because the issue has never been dispatched or because cleanup deleted the ref.
    ALTER TABLE issue_state ADD COLUMN branch TEXT;
    "#,
    // v5
    r#"
    -- Where this run's raw event stream was written. Nullable because a transcript is
    -- best-effort: a run dispatched with transcripts off, or whose file could not be opened,
    -- still gets a row. Recording the path rather than deriving it is what makes "show me what
    -- run X did" answerable from the run record alone, without knowing the layout on disk.
    ALTER TABLE run ADD COLUMN transcript TEXT;
    "#,
    // v6
    r#"
    -- Delivery: what happened to an issue's branch after a run reported done. One row per
    -- issue, because one issue has one branch and therefore at most one open pull request at
    -- a time. The row is a cache of judgment like everything else here — losing it costs a
    -- re-push and a re-read of the pull request, both idempotent — with one exception that is
    -- the reason two of these counters exist: `rounds_issue` is the bound on how many times
    -- delivery may hand an issue back to an agent over its whole life, and a bound that reset
    -- with every fresh run (or every fresh pull request, which is what `rounds_pr` tracks)
    -- would bound nothing, for the same reason `max_calls_per_run` alone bounds no broker.
    CREATE TABLE IF NOT EXISTS delivery (
      issue_id          TEXT PRIMARY KEY REFERENCES issue_state(issue_id),
      stage             TEXT NOT NULL,
      pr_number         INTEGER,
      pr_url            TEXT,
      base              TEXT,
      head_sha          TEXT,
      head_pushed_at    INTEGER,
      review_requested  INTEGER NOT NULL DEFAULT 0,
      review_error      TEXT,
      rounds_pr         INTEGER NOT NULL DEFAULT 0,
      rounds_issue      INTEGER NOT NULL DEFAULT 0,
      pending_feedback  TEXT,
      pending_verdicts  TEXT,
      -- Comment ids handed to the most recent fix round, so the next one can name the threads
      -- a run was told about and left unanswered, rather than presenting them as new.
      handed_comments   TEXT,
      handoff_reason    TEXT,
      updated_at        INTEGER NOT NULL
    );

    -- Every review comment that has been settled, and how. Keyed by the provider's comment id
    -- so a later round can be handed only the threads still open, and so nobody re-argues one
    -- already accepted or rejected. `detail` is the commit that resolved it or the reason it
    -- was declined — never empty, because a verdict without its grounds is just a label.
    CREATE TABLE IF NOT EXISTS review_verdict (
      issue_id     TEXT NOT NULL REFERENCES issue_state(issue_id),
      comment_id   TEXT NOT NULL,
      pr_number    INTEGER NOT NULL,
      verdict      TEXT NOT NULL,
      detail       TEXT NOT NULL,
      recorded_at  INTEGER NOT NULL,
      PRIMARY KEY (issue_id, comment_id)
    );
    "#,
    // v7
    r#"
    -- Consecutive handoff-gate failures (#46). Held only in memory, a restart forgave the
    -- streak while nothing about the issue had changed, so `gate.max_failures` bounded failures
    -- per process rather than per line of work — and a suite the agent cannot make pass is the
    -- case both most in need of the bound and most likely to outlive a restart.
    ALTER TABLE issue_state ADD COLUMN gate_failures INTEGER NOT NULL DEFAULT 0;
    "#,
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            apply(conn, sql, version)?;
        }
    }
    Ok(())
}

/// One migration and the version bump that records it, in a single transaction.
///
/// Both halves or neither. `execute_batch` runs in autocommit, so a rebuild like v3 — several
/// statements, one of them a `DROP TABLE` — could be interrupted between them and leave a
/// half-built schema still labelled with the *old* version. The next startup would then retry
/// that migration against a database it had already half-changed, fail on the first `CREATE`,
/// and return an error out of `migrate`, so the store would refuse to open at all. That breaks
/// the contract this store is built on: losing it may cost re-polling, but it must never stop
/// the process from starting. `PRAGMA user_version` is a header write and rolls back with
/// everything else, which is what makes the version safe to set inside the same transaction.
fn apply(conn: &Connection, sql: &str, version: i64) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(sql)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_migration_that_fails_partway_leaves_no_trace_and_does_not_advance_the_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let before: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();

        // Shaped like v3: build something, then fail before the end of the batch.
        let half = "CREATE TABLE half_done (x INTEGER); INSERT INTO nonexistent VALUES (1);";
        assert!(apply(&conn, half, before + 1).is_err());

        let after: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(after, before, "a migration that failed must not claim to have applied");

        let leftover: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'half_done'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(leftover, 0, "nor leave the half it did finish behind for the next startup");
    }
}
