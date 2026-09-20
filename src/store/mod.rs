//! Durable scheduler state.
//!
//! This is a cache of *judgment*, not a system of record. The tracker owns what work exists and
//! the filesystem owns the workspaces; SQLite holds only what cannot be re-derived — attempt
//! counts, quarantine flags, turn budgets. Delete the file and the service degrades to stateless
//! re-polling, which is dumber but still correct. That property is what makes adding a database
//! safe here, where the spec left the retry queue unpersisted (§18.2, listed as an open TODO).

pub mod schema;

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::clock::{Clock, Wall};
use crate::model::{ErrorClass, Phase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueState {
    pub issue_id: String,
    pub identifier: String,
    pub worktree_key: String,
    pub phase: Phase,
    pub attempt: u32,
    pub consecutive_fail: u32,
    pub last_fail_class: Option<ErrorClass>,
    pub cumulative_turns: u32,
    pub miss_count: u32,
    /// Tracker state this issue was parked in after a `Done`/`Blocked` verdict, if any.
    pub parked_state: Option<String>,
    pub quarantined_at: Option<i64>,
    pub last_error_class: Option<ErrorClass>,
    pub last_error: Option<String>,
    pub task_ref: Option<String>,
    /// The conversation continuations for this issue resume into, once one has been named.
    pub session_id: Option<String>,
}

impl IssueState {
    pub fn is_quarantined(&self) -> bool {
        self.quarantined_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryEntry {
    pub issue_id: String,
    pub due_at: i64,
    pub attempt: u32,
    pub reason: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        schema::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Insert-if-absent. Returns the current row either way.
    pub fn ensure(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        identifier: &str,
        worktree_key: &str,
    ) -> rusqlite::Result<IssueState> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO issue_state (issue_id, identifier, worktree_key, phase, updated_at)
             VALUES (?1, ?2, ?3, 'released', ?4)
             ON CONFLICT(issue_id) DO UPDATE SET identifier = ?2, updated_at = ?4",
            params![issue_id, identifier, worktree_key, clock.wall().0],
        )?;
        Self::get_locked(&conn, issue_id).map(|o| o.expect("row inserted above"))
    }

    pub fn get(&self, issue_id: &str) -> rusqlite::Result<Option<IssueState>> {
        let conn = self.conn.lock().unwrap();
        Self::get_locked(&conn, issue_id)
    }

    fn get_locked(conn: &Connection, issue_id: &str) -> rusqlite::Result<Option<IssueState>> {
        conn.query_row(
            "SELECT issue_id, identifier, worktree_key, phase, attempt, consecutive_fail,
                    last_fail_class, cumulative_turns, miss_count, parked_state, quarantined_at,
                    last_error_class, last_error, task_ref, session_id
             FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            Self::row_to_state,
        )
        .optional()
    }

    fn row_to_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<IssueState> {
        let phase: String = row.get(3)?;
        let last_fail: Option<String> = row.get(6)?;
        let last_err_class: Option<String> = row.get(11)?;
        Ok(IssueState {
            issue_id: row.get(0)?,
            identifier: row.get(1)?,
            worktree_key: row.get(2)?,
            phase: match phase.as_str() {
                "queued" => Phase::Queued,
                "running" => Phase::Running,
                "retry" => Phase::RetryQueued,
                "quarantine" => Phase::Quarantined,
                _ => Phase::Released,
            },
            attempt: row.get::<_, i64>(4)? as u32,
            consecutive_fail: row.get::<_, i64>(5)? as u32,
            last_fail_class: last_fail.as_deref().and_then(ErrorClass::parse),
            cumulative_turns: row.get::<_, i64>(7)? as u32,
            miss_count: row.get::<_, i64>(8)? as u32,
            parked_state: row.get(9)?,
            quarantined_at: row.get(10)?,
            last_error_class: last_err_class.as_deref().and_then(ErrorClass::parse),
            last_error: row.get(12)?,
            task_ref: row.get(13)?,
            session_id: row.get(14)?,
        })
    }

    pub fn all(&self) -> rusqlite::Result<Vec<IssueState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, identifier, worktree_key, phase, attempt, consecutive_fail,
                    last_fail_class, cumulative_turns, miss_count, parked_state, quarantined_at,
                    last_error_class, last_error, task_ref, session_id
             FROM issue_state ORDER BY identifier",
        )?;
        let rows = stmt.query_map([], Self::row_to_state)?;
        rows.collect()
    }

    pub fn set_phase(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        phase: Phase,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET phase = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, phase.label(), clock.wall().0],
        )?;
        Ok(())
    }

    /// Reserve an issue. Returns false if it is already claimed — this is the guard that makes
    /// duplicate dispatch impossible, and it must commit *before* a worker is spawned.
    pub fn claim(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE issue_state SET phase = 'running', updated_at = ?2
             WHERE issue_id = ?1 AND phase NOT IN ('running', 'quarantine')",
            params![issue_id, clock.wall().0],
        )?;
        Ok(n == 1)
    }

    pub fn release(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state
             SET phase = 'released', attempt = 0, consecutive_fail = 0, last_fail_class = NULL,
                 miss_count = 0, updated_at = ?2
             WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        Ok(())
    }

    /// Record a failure and decide whether it quarantines the issue.
    ///
    /// Permanent classes quarantine immediately. Retryable classes quarantine after
    /// `quarantine_after` consecutive failures *of the same class* — a deterministic fault
    /// wearing a transient mask still gets caught. A different class resets the streak.
    pub fn record_failure(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        class: ErrorClass,
        msg: &str,
        quarantine_after: u32,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = clock.wall().0;

        let prev: Option<String> = conn
            .query_row(
                "SELECT last_fail_class FROM issue_state WHERE issue_id = ?1",
                params![issue_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let same_class = prev.as_deref().and_then(ErrorClass::parse) == Some(class);
        let streak_sql = if same_class { "consecutive_fail + 1" } else { "1" };

        conn.execute(
            &format!(
                "UPDATE issue_state
                 SET attempt = attempt + 1,
                     consecutive_fail = {streak_sql},
                     last_fail_class = ?2,
                     last_error_class = ?2,
                     last_error = ?3,
                     updated_at = ?4
                 WHERE issue_id = ?1"
            ),
            params![issue_id, class.as_str(), msg, now],
        )?;

        let streak: u32 = conn.query_row(
            "SELECT consecutive_fail FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )?;

        let quarantine = !class.retryable() || streak >= quarantine_after;
        if quarantine {
            conn.execute(
                "UPDATE issue_state SET quarantined_at = ?2, phase = 'quarantine', updated_at = ?2
                 WHERE issue_id = ?1",
                params![issue_id, now],
            )?;
            conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        }
        Ok(quarantine)
    }

    pub fn unquarantine(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state
             SET quarantined_at = NULL, phase = 'released', attempt = 0, consecutive_fail = 0,
                 last_fail_class = NULL, updated_at = ?2
             WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Park an issue in its current tracker state after a `Done` or `Blocked` verdict.
    ///
    /// Releasing the claim alone is not enough: the ticket is usually still sitting in an
    /// active state, so the very next tick would pick it straight back up. That is the spec's
    /// continuation runaway arriving by a different route. Parking says "nothing has changed
    /// since we last finished with this", and lifts the moment the state moves.
    pub fn park(&self, clock: &dyn Clock, issue_id: &str, state_key: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET parked_state = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, state_key, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn unpark(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET parked_state = NULL, updated_at = ?2 WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Name the conversation this issue's continuations resume into, or clear it.
    ///
    /// Clearing is the degradation path: a resume target the CLI no longer knows about fails
    /// every attempt identically, so the run that discovers it drops the id and the next
    /// attempt starts fresh instead of retrying its way into quarantine.
    pub fn set_session(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        session_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET session_id = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, session_id, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn add_turns(&self, issue_id: &str, turns: u32) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET cumulative_turns = cumulative_turns + ?2 WHERE issue_id = ?1",
            params![issue_id, turns as i64],
        )?;
        Ok(())
    }

    /// Returns the miss count after incrementing.
    pub fn bump_miss(&self, issue_id: &str) -> rusqlite::Result<u32> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET miss_count = miss_count + 1 WHERE issue_id = ?1",
            params![issue_id],
        )?;
        conn.query_row(
            "SELECT miss_count FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )
    }

    pub fn clear_miss(&self, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET miss_count = 0 WHERE issue_id = ?1",
            params![issue_id],
        )?;
        Ok(())
    }

    pub fn schedule_retry(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        due_at: Wall,
        attempt: u32,
        reason: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO retry (issue_id, due_at, attempt, reason) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(issue_id) DO UPDATE SET due_at = ?2, attempt = ?3, reason = ?4",
            params![issue_id, due_at.0, attempt as i64, reason],
        )?;
        conn.execute(
            "UPDATE issue_state SET phase = 'retry', updated_at = ?2 WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn due_retries(&self, now: Wall) -> rusqlite::Result<Vec<RetryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, due_at, attempt, reason FROM retry WHERE due_at <= ?1 ORDER BY due_at",
        )?;
        let rows = stmt.query_map(params![now.0], |r| {
            Ok(RetryEntry {
                issue_id: r.get(0)?,
                due_at: r.get(1)?,
                attempt: r.get::<_, i64>(2)? as u32,
                reason: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn all_retries(&self) -> rusqlite::Result<Vec<RetryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT issue_id, due_at, attempt, reason FROM retry ORDER BY due_at")?;
        let rows = stmt.query_map([], |r| {
            Ok(RetryEntry {
                issue_id: r.get(0)?,
                due_at: r.get(1)?,
                attempt: r.get::<_, i64>(2)? as u32,
                reason: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn clear_retry(&self, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        Ok(())
    }

    pub fn start_run(
        &self,
        clock: &dyn Clock,
        run_id: &str,
        issue_id: &str,
        session_id: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run (run_id, issue_id, started_at, session_id) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, issue_id, clock.wall().0, session_id],
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        clock: &dyn Clock,
        run_id: &str,
        outcome: &str,
        turns: u32,
        in_tok: u64,
        out_tok: u64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE run SET ended_at = ?2, outcome = ?3, turns = ?4, in_tok = ?5, out_tok = ?6
             WHERE run_id = ?1",
            params![run_id, clock.wall().0, outcome, turns as i64, in_tok as i64, out_tok as i64],
        )?;
        Ok(())
    }

    pub fn token_totals(&self) -> rusqlite::Result<(u64, u64)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(in_tok),0), COALESCE(SUM(out_tok),0) FROM run",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn setup() -> (Store, FakeClock) {
        let s = Store::open_in_memory().unwrap();
        let c = FakeClock::new();
        s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
        (s, c)
    }

    #[test]
    fn claim_is_exclusive() {
        let (s, c) = setup();
        assert!(s.claim(&c, "id-1").unwrap());
        assert!(!s.claim(&c, "id-1").unwrap(), "second claim must fail");
        s.release(&c, "id-1").unwrap();
        assert!(s.claim(&c, "id-1").unwrap());
    }

    #[test]
    fn permanent_failure_quarantines_on_first_occurrence() {
        let (s, c) = setup();
        let q = s.record_failure(&c, "id-1", ErrorClass::TemplateRender, "bad var", 3).unwrap();
        assert!(q);
        assert!(s.get("id-1").unwrap().unwrap().is_quarantined());
    }

    #[test]
    fn retryable_failures_quarantine_only_after_the_configured_streak() {
        let (s, c) = setup();
        for i in 1..=2 {
            let q = s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap();
            assert!(!q, "quarantined early at {i}");
        }
        assert!(s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap());
    }

    #[test]
    fn a_different_failure_class_resets_the_streak() {
        let (s, c) = setup();
        s.record_failure(&c, "id-1", ErrorClass::Stall, "a", 3).unwrap();
        s.record_failure(&c, "id-1", ErrorClass::Stall, "b", 3).unwrap();
        // Different class: streak restarts at 1 rather than tipping into quarantine.
        let q = s.record_failure(&c, "id-1", ErrorClass::TurnTimeout, "c", 3).unwrap();
        assert!(!q);
        let st = s.get("id-1").unwrap().unwrap();
        assert_eq!(st.consecutive_fail, 1);
        assert_eq!(st.attempt, 3, "attempt counts every failure regardless of class");
    }

    #[test]
    fn quarantined_issues_cannot_be_claimed_until_cleared() {
        let (s, c) = setup();
        s.record_failure(&c, "id-1", ErrorClass::AuthFailed, "401", 3).unwrap();
        assert!(!s.claim(&c, "id-1").unwrap());
        s.unquarantine(&c, "id-1").unwrap();
        assert!(s.claim(&c, "id-1").unwrap());
    }

    #[test]
    fn retries_become_due_only_once_the_clock_reaches_them() {
        let (s, c) = setup();
        let due = Wall(c.wall().0 + 10_000);
        s.schedule_retry(&c, "id-1", due, 1, "backoff").unwrap();
        assert!(s.due_retries(c.wall()).unwrap().is_empty());
        c.advance_ms(10_000);
        assert_eq!(s.due_retries(c.wall()).unwrap().len(), 1);
    }

    #[test]
    fn state_survives_reopening_the_same_database() {
        let dir = std::env::temp_dir().join(format!("symphony-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let _ = std::fs::remove_file(&path);

        let c = FakeClock::new();
        {
            let s = Store::open(&path).unwrap();
            s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
            s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap();
        }
        {
            let s = Store::open(&path).unwrap();
            let st = s.get("id-1").unwrap().unwrap();
            assert_eq!(st.attempt, 1, "attempt counter must survive restart");
            assert_eq!(st.last_fail_class, Some(ErrorClass::Stall));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
