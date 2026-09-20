//! Projection of scheduler state into an external, human-readable surface.
//!
//! One-way by contract: the orchestrator writes, and never reads back to make a decision. The
//! target is `~/.claude/tasks/<session-id>/`, one JSON file per issue. The observed shape on
//! this machine is `{id, subject, description, activeForm, status, blocks, blockedBy}` — but it
//! is Claude Code's internal store with no published schema, so [`TasksProjector`] probes a
//! sample file at startup rather than assuming that shape still holds. Keeping the flow one-way
//! means a schema change costs a dashboard, not the scheduler.
//!
//! Slice 1 shipped the seam and [`NoopProjector`]. Slice 2 adds the real writer.

use crate::model::Phase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedIssue {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: Option<String>,
    pub tracker_state: String,
    pub phase: Phase,
    pub attempt: u32,
    pub cumulative_turns: u32,
    pub in_tok: u64,
    pub out_tok: u64,
    pub workspace: Option<String>,
    pub retry_due_at: Option<i64>,
    pub quarantined: bool,
    pub last_error: Option<String>,
    /// Dispatch ids this issue waits on, rendered as task dependencies.
    pub blocked_by: Vec<String>,
}

pub trait Projector: Send + Sync {
    /// Best-effort. An error here is logged and ignored: this is a view, never a dependency.
    fn project(&self, issues: &[ProjectedIssue]) -> anyhow::Result<()>;
}

pub struct NoopProjector;

impl Projector for NoopProjector {
    fn project(&self, _issues: &[ProjectedIssue]) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The field set observed on `~/.claude/tasks/<session>/<n>.json` on this machine. Not a
/// published schema — treat drift as a reason to stop writing, not a reason to guess.
const EXPECTED_TASK_KEYS: &[&str] =
    &["id", "subject", "description", "activeForm", "status", "blocks", "blockedBy"];

/// Writes one JSON file per issue into `<tasks_root>/<session_id>/`, in the same shape Claude
/// Code's own task files use.
///
/// One-way in the sense that matters: the only read this type performs is the startup schema
/// probe, and that feeds a warning and an on/off switch for *this type's own writes* — never a
/// value the scheduler sees. `blockedBy`/`blocks` are the sole exception to "write and forget":
/// they are computed from the full issue list on every call, but only from the batch handed to
/// this `project()` call, never from anything already on disk.
pub struct TasksProjector {
    session_dir: std::path::PathBuf,
    enabled: bool,
}

impl TasksProjector {
    /// `tasks_root` is typically `~/.claude/tasks`. `session_id` should be stable across
    /// restarts of the same deployment — see [`derive_session_id`] — so a restart updates one
    /// directory instead of littering a fresh one every time.
    pub fn new(tasks_root: &std::path::Path, session_id: &str) -> anyhow::Result<Self> {
        let session_dir = tasks_root.join(session_id);
        std::fs::create_dir_all(&session_dir)?;

        let enabled = Self::probe(tasks_root, &session_dir);
        if !enabled {
            tracing::warn!(
                tasks_root = %tasks_root.display(),
                "an existing task file's shape does not match what this projector expects; \
                 disabling the projection rather than writing a guessed shape"
            );
        }
        Ok(Self { session_dir, enabled })
    }

    /// Look at one already-written task file from *another* session directory and check it
    /// carries every key this projector relies on. Never inspects this projector's own output,
    /// and its result never reaches a scheduling decision — only whether this type keeps
    /// writing.
    fn probe(tasks_root: &std::path::Path, own_dir: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(tasks_root) else {
            return true; // nothing on disk yet to contradict our assumptions
        };

        let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || dir == own_dir {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&dir) else { continue };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(modified) = f.metadata().and_then(|m| m.modified()) else { continue };
                if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                    newest = Some((modified, path));
                }
            }
        }

        let Some((_, sample)) = newest else {
            return true; // no sample anywhere; proceed optimistically
        };
        let Ok(text) = std::fs::read_to_string(&sample) else { return true };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return true };
        let Some(obj) = value.as_object() else { return false };

        EXPECTED_TASK_KEYS.iter().all(|k| obj.contains_key(*k))
    }

    /// Stable per-issue filename and `id` value: keyed off the dispatch id so it survives a
    /// restart and matches the same suffix `worktree_key` uses for the same issue.
    fn slug(issue_id: &str) -> String {
        blake3::hash(issue_id.as_bytes()).to_hex().chars().take(12).collect()
    }

    fn status_of(phase: Phase) -> &'static str {
        match phase {
            Phase::Running => "in_progress",
            Phase::Released => "completed",
            Phase::Queued | Phase::RetryQueued | Phase::Quarantined => "pending",
        }
    }

    fn active_form(issue: &ProjectedIssue) -> String {
        match issue.phase {
            Phase::Running => format!("Working on {}", issue.identifier),
            Phase::RetryQueued => format!("Waiting to retry {}", issue.identifier),
            Phase::Queued => format!("Queued: {}", issue.identifier),
            Phase::Quarantined => format!("Quarantined: {}", issue.identifier),
            Phase::Released => format!("Finished {}", issue.identifier),
        }
    }

    /// Everything that has no field of its own in the schema — tokens, workspace path, tracker
    /// url, retry timing, the last error — goes into this free-text blob, since `description`
    /// is the one field with room for it.
    fn describe(issue: &ProjectedIssue) -> String {
        let mut lines = vec![
            format!("tracker state: {}", issue.tracker_state),
            format!("phase: {}", issue.phase.label()),
            format!("attempt: {}", issue.attempt),
            format!("turns: {}", issue.cumulative_turns),
            format!("tokens: {} in / {} out", issue.in_tok, issue.out_tok),
        ];
        if let Some(ws) = &issue.workspace {
            lines.push(format!("workspace: {ws}"));
        }
        if let Some(url) = &issue.url {
            lines.push(format!("url: {url}"));
        }
        if let Some(due) = issue.retry_due_at {
            lines.push(format!("retry due at: {due}"));
        }
        if issue.quarantined {
            lines.push("quarantined".into());
        }
        if let Some(err) = &issue.last_error {
            lines.push(format!("last error: {err}"));
        }
        lines.join("\n")
    }
}

impl Projector for TasksProjector {
    fn project(&self, issues: &[ProjectedIssue]) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // `ProjectedIssue` only carries `blocked_by`; `blocks` is the inverse edge, computed
        // fresh from this call's batch so it never depends on a previous write.
        let mut blocks: std::collections::HashMap<&str, Vec<String>> = Default::default();
        for issue in issues {
            for dep in &issue.blocked_by {
                blocks.entry(dep.as_str()).or_default().push(Self::slug(&issue.issue_id));
            }
        }
        let present: std::collections::HashSet<&str> =
            issues.iter().map(|i| i.issue_id.as_str()).collect();

        let mut written = std::collections::HashSet::new();
        for issue in issues {
            let id = Self::slug(&issue.issue_id);
            let task = serde_json::json!({
                "id": id,
                "subject": format!("{}: {}", issue.identifier, issue.title),
                "description": Self::describe(issue),
                "activeForm": Self::active_form(issue),
                "status": Self::status_of(issue.phase),
                "blocks": blocks.get(issue.issue_id.as_str()).cloned().unwrap_or_default(),
                // A blocker not present in this tick's batch is dropped rather than left
                // dangling — a temporarily invisible dependency costs an edge on this view,
                // never a wrong scheduling decision, since this file is never read back.
                "blockedBy": issue
                    .blocked_by
                    .iter()
                    .filter(|b| present.contains(b.as_str()))
                    .map(|b| Self::slug(b))
                    .collect::<Vec<_>>(),
            });
            let path = self.session_dir.join(format!("{id}.json"));
            std::fs::write(&path, serde_json::to_vec_pretty(&task)?)?;
            written.insert(id);
        }

        // Issues that dropped out of this tick's batch lose their file. In practice this
        // rarely fires — issue_state rows are never deleted, so a released issue keeps
        // appearing as `Phase::Released` forever — but a projector that never reconciles
        // against its own directory is a projector that can only grow.
        if let Ok(entries) = std::fs::read_dir(&self.session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                if path.extension().and_then(|e| e.to_str()) == Some("json")
                    && !written.contains(stem)
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        Ok(())
    }
}

/// Derives a stable, UUID-shaped directory name from a seed (typically the canonicalized
/// workspace root), so the same deployment always maps to the same session directory across
/// restarts without persisting an id anywhere.
pub fn derive_session_id(seed: &str) -> String {
    let hex: String = blake3::hash(seed.as_bytes()).to_hex().chars().take(32).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "symphony-proj-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn issue(id: &str, identifier: &str, phase: Phase) -> ProjectedIssue {
        ProjectedIssue {
            issue_id: id.into(),
            identifier: identifier.into(),
            title: "some work".into(),
            url: Some("https://tracker.example/1".into()),
            tracker_state: "In Progress".into(),
            phase,
            attempt: 1,
            cumulative_turns: 3,
            in_tok: 100,
            out_tok: 50,
            workspace: Some("/tmp/ws/x".into()),
            retry_due_at: None,
            quarantined: false,
            last_error: None,
            blocked_by: vec![],
        }
    }

    #[test]
    fn derived_session_ids_are_stable_and_distinct() {
        assert_eq!(derive_session_id("a"), derive_session_id("a"));
        assert_ne!(derive_session_id("a"), derive_session_id("b"));
        assert_eq!(derive_session_id("a").len(), 36, "should look like a UUID");
    }

    #[test]
    fn projecting_writes_one_file_per_issue_in_the_expected_shape() {
        let root = tmp_dir("shape");
        let projector = TasksProjector::new(&root, "sess-1").unwrap();

        let issues =
            vec![issue("id-1", "MT-1", Phase::Running), issue("id-2", "MT-2", Phase::Released)];
        projector.project(&issues).unwrap();

        let dir = root.join("sess-1");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 2);

        for f in files {
            let text = std::fs::read_to_string(f.path()).unwrap();
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            for key in EXPECTED_TASK_KEYS {
                assert!(v.get(key).is_some(), "missing {key} in {text}");
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn phase_maps_onto_the_three_task_statuses() {
        assert_eq!(TasksProjector::status_of(Phase::Running), "in_progress");
        assert_eq!(TasksProjector::status_of(Phase::Released), "completed");
        assert_eq!(TasksProjector::status_of(Phase::Queued), "pending");
        assert_eq!(TasksProjector::status_of(Phase::RetryQueued), "pending");
        assert_eq!(TasksProjector::status_of(Phase::Quarantined), "pending");
    }

    #[test]
    fn a_blocker_missing_from_the_batch_is_dropped_not_left_dangling() {
        let root = tmp_dir("blockers");
        let projector = TasksProjector::new(&root, "sess-1").unwrap();

        let mut blocked = issue("id-2", "MT-2", Phase::Queued);
        blocked.blocked_by = vec!["id-1".into(), "id-ghost".into()];
        let blocker = issue("id-1", "MT-1", Phase::Running);

        projector.project(&[blocker.clone(), blocked.clone()]).unwrap();

        let dir = root.join("sess-1");
        let blocked_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{}.json", TasksProjector::slug("id-2"))))
                .unwrap(),
        )
        .unwrap();
        let blocked_by = blocked_json["blockedBy"].as_array().unwrap();
        assert_eq!(blocked_by.len(), 1, "the ghost blocker must not appear");
        assert_eq!(blocked_by[0], TasksProjector::slug("id-1"));

        let blocker_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{}.json", TasksProjector::slug("id-1"))))
                .unwrap(),
        )
        .unwrap();
        let blocks = blocker_json["blocks"].as_array().unwrap();
        assert_eq!(blocks[0], TasksProjector::slug("id-2"), "blocks is the inverse edge");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_dropped_issue_loses_its_file_on_the_next_projection() {
        let root = tmp_dir("drop");
        let projector = TasksProjector::new(&root, "sess-1").unwrap();

        projector.project(&[issue("id-1", "MT-1", Phase::Running)]).unwrap();
        let dir = root.join("sess-1");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        projector.project(&[]).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_sample_missing_an_expected_key_disables_the_projector() {
        let root = tmp_dir("bad-schema");
        let other = root.join("some-other-session");
        std::fs::create_dir_all(&other).unwrap();
        // Missing `blockedBy`: a plausible future shape this projector must not guess at.
        std::fs::write(
            other.join("1.json"),
            r#"{"id":"1","subject":"x","description":"","activeForm":"x","status":"pending","blocks":[]}"#,
        )
        .unwrap();

        let projector = TasksProjector::new(&root, "sess-1").unwrap();
        projector.project(&[issue("id-1", "MT-1", Phase::Running)]).unwrap();

        let dir = root.join("sess-1");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a projector that saw a mismatched schema must not write anything"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_matching_sample_leaves_the_projector_enabled() {
        let root = tmp_dir("good-schema");
        let other = root.join("some-other-session");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("1.json"),
            r#"{"id":"1","subject":"x","description":"","activeForm":"x","status":"pending","blocks":[],"blockedBy":[]}"#,
        )
        .unwrap();

        let projector = TasksProjector::new(&root, "sess-1").unwrap();
        projector.project(&[issue("id-1", "MT-1", Phase::Running)]).unwrap();

        let dir = root.join("sess-1");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_sample_anywhere_proceeds_optimistically() {
        let root = tmp_dir("no-sample");
        let projector = TasksProjector::new(&root, "sess-1").unwrap();
        projector.project(&[issue("id-1", "MT-1", Phase::Running)]).unwrap();

        let dir = root.join("sess-1");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
