//! [`Gate`] over real `git rebase` and real subprocesses, in the run's own worktree.
//!
//! One supervising thread per gate, the same shape as the real worker: the scheduler polls a
//! handle, and the thread does the blocking. Commands are exec'd directly — argv, no shell —
//! for the reason the worker never uses `bash -lc`: a shell re-imports the operator's dotfiles,
//! and a gate command is the one subprocess here that inherits the operator's environment, so it
//! is also the one that must not be handed a shell to widen that further. A command that needs a
//! pipe belongs in a script the repository checks in.
//!
//! The environment *is* inherited, unlike the worker's allowlist: this is the operator's own
//! test suite running in the operator's own repository with no agent involved, and `cargo`
//! needs `PATH`, `CARGO_HOME`, `RUSTUP_HOME` and whatever else the machine's toolchain relies on.
//! Colour is switched off because the output's destination is a prompt, not a terminal.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{Gate, GateHandle, Verdict};
use crate::model::Issue;
use crate::worker::KillResult;

/// How much of a failing command's output travels back to the agent. The tail, not the head:
/// `cargo test` prints its `failures:` section and summary last, and a compiler stops at the
/// last error. 16 KiB is a few screens — enough to act on, small enough not to crowd the
/// conversation it lands in.
const OUTPUT_CAP: usize = 16 * 1024;

pub struct GitGate {
    repo: PathBuf,
    /// The ref to rebase onto, resolved in `repo`. `None` means `repo`'s own HEAD — the same
    /// commit `GitWorktreeWorkspace::prepare` branches from, now rather than then.
    base: Option<String>,
    commands: Vec<Vec<String>>,
}

impl GitGate {
    pub fn new(repo: impl Into<PathBuf>, base: Option<String>, commands: Vec<Vec<String>>) -> Self {
        Self { repo: repo.into(), base, commands }
    }
}

#[derive(Default)]
struct Inner {
    step: String,
    verdict: Option<Verdict>,
    /// Process group of the command currently running, so `kill` can reach it — and every
    /// grandchild `cargo test` spawns — from another thread.
    pgid: Option<i32>,
    killed: bool,
}

type Shared = Arc<(Mutex<Inner>, Condvar)>;

struct GitGateRun {
    state: Shared,
}

impl GateHandle for GitGateRun {
    fn finished(&self) -> Option<Verdict> {
        self.state.0.lock().unwrap().verdict.clone()
    }

    fn step(&self) -> String {
        self.state.0.lock().unwrap().step.clone()
    }

    fn kill(&self, grace_ms: u64) -> KillResult {
        let (lock, cvar) = &*self.state;
        let pgid = {
            let mut g = lock.lock().unwrap();
            if g.verdict.is_some() {
                return KillResult::AlreadyDone;
            }
            g.killed = true;
            g.pgid
        };
        // The git steps run to completion on their own — they are short — and the thread checks
        // `killed` between steps. Only a gate command is worth signalling.
        if let Some(pgid) = pgid {
            unsafe { libc::kill(-pgid, libc::SIGTERM) };
        }
        let g = lock.lock().unwrap();
        let (g, timeout) = cvar
            .wait_timeout_while(g, Duration::from_millis(grace_ms), |g| g.verdict.is_none())
            .unwrap();
        if !timeout.timed_out() {
            return KillResult::Stopped;
        }
        let pgid = g.pgid;
        drop(g);
        if let Some(pgid) = pgid {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
        let g = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(g, Duration::from_secs(5), |g| g.verdict.is_none());
        KillResult::Forced
    }
}

impl Gate for GitGate {
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle> {
        let state: Shared = Arc::new((Mutex::new(Inner::default()), Condvar::new()));
        let run = GateRun {
            state: state.clone(),
            repo: self.repo.clone(),
            base: self.base.clone(),
            commands: self.commands.clone(),
            workspace: workspace.to_path_buf(),
            identifier: issue.identifier.clone(),
        };
        std::thread::Builder::new()
            .name(format!("gate-{}", issue.identifier))
            .spawn(move || run.run())
            .expect("spawning a gate thread");
        Arc::new(GitGateRun { state })
    }
}

/// Everything the supervising thread owns.
struct GateRun {
    state: Shared,
    repo: PathBuf,
    base: Option<String>,
    commands: Vec<Vec<String>>,
    workspace: PathBuf,
    identifier: String,
}

impl GateRun {
    fn set_step(&self, step: impl Into<String>) {
        self.state.0.lock().unwrap().step = step.into();
    }

    fn killed(&self) -> bool {
        self.state.0.lock().unwrap().killed
    }

    fn finish(&self, verdict: Verdict) {
        let (lock, cvar) = &*self.state;
        lock.lock().unwrap().verdict = Some(verdict);
        cvar.notify_all();
    }

    fn run(self) {
        let verdict = self.gate();
        self.finish(verdict);
    }

    fn gate(&self) -> Verdict {
        let base_label = self.base.as_deref().unwrap_or("HEAD");
        let ws = &self.workspace;

        // Resolved in `repo`, not in the worktree: the worktree's HEAD is the run's own branch,
        // and a bare `HEAD` there would rebase the branch onto itself and call it current.
        self.set_step(format!("resolve base {base_label}"));
        let base_sha = match git(
            &self.repo,
            &["rev-parse", "--verify", "--quiet", &format!("{base_label}^{{commit}}")],
        ) {
            Ok(sha) => sha,
            Err(e) => {
                return Verdict::Failed {
                    step: format!("resolve base {base_label}"),
                    output: format!(
                        "cannot resolve rebase base `{base_label}` in {}: {e}",
                        self.repo.display()
                    ),
                };
            }
        };

        let ahead = git(ws, &["rev-list", "--count", &format!("{base_sha}..HEAD")])
            .ok()
            .and_then(|n| n.parse::<u64>().ok());
        match ahead {
            Some(0) => return Verdict::NoCommits,
            Some(_) => {}
            None => {
                return Verdict::Failed {
                    step: "count commits".into(),
                    output: format!("{} is not a git checkout the gate can read", ws.display()),
                };
            }
        }
        if self.killed() {
            return stopped("count commits");
        }

        let step = format!("rebase onto {base_label}");
        self.set_step(&step);
        let before = git(ws, &["rev-parse", "HEAD"]).unwrap_or_default();
        if let Err(stderr) = git(ws, &["rebase", &base_sha]) {
            // Conflicted paths are read *before* the abort, which is what clears them. The
            // abort itself is what makes a conflict safe to report: the branch goes back to
            // exactly the commits the agent made, so `Workspace::remove`'s merged check still
            // sees work and keeps it, and a human inherits a clean, if stale, branch.
            let paths: Vec<String> = git(ws, &["diff", "--name-only", "--diff-filter=U"])
                .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
                .unwrap_or_default();
            let _ = git(ws, &["rebase", "--abort"]);
            if !paths.is_empty() {
                return Verdict::Conflict { paths };
            }
            return Verdict::Failed { step, output: stderr };
        }
        let after = git(ws, &["rev-parse", "HEAD"]).unwrap_or_default();
        let rebased = before != after;
        tracing::debug!(issue = %self.identifier, base = base_label, rebased, "rebase clean");

        for argv in &self.commands {
            if self.killed() {
                return stopped(argv.join(" "));
            }
            let step = argv.join(" ");
            self.set_step(&step);
            if let Err(output) = self.run_command(argv) {
                return Verdict::Failed { step, output };
            }
        }
        Verdict::Passed { rebased }
    }

    /// One gate command, to completion. `Err` carries what the agent should see.
    fn run_command(&self, argv: &[String]) -> Result<(), String> {
        let (bin, args) = argv.split_first().ok_or_else(|| "empty command".to_string())?;
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(&self.workspace)
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so `kill` reaches the test binaries `cargo test` forks and
        // not just `cargo`.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn().map_err(|e| format!("could not start `{bin}`: {e}"))?;
        let pid = child.id() as i32;
        self.state.0.lock().unwrap().pgid = Some(pid);

        // Reads both pipes without either deadlocking the other; blocks until exit, which is
        // what the supervising thread is for.
        let out = child.wait_with_output();
        self.state.0.lock().unwrap().pgid = None;
        let out = out.map_err(|e| format!("waiting for `{bin}`: {e}"))?;

        if out.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut combined = format!("{} exited with {}\n", argv.join(" "), out.status);
        if !stdout.trim().is_empty() {
            combined.push_str("--- stdout ---\n");
            combined.push_str(stdout.trim_end());
            combined.push('\n');
        }
        if !stderr.trim().is_empty() {
            combined.push_str("--- stderr ---\n");
            combined.push_str(stderr.trim_end());
            combined.push('\n');
        }
        Err(tail(&combined, OUTPUT_CAP))
    }
}

fn stopped(step: impl Into<String>) -> Verdict {
    Verdict::Failed { step: step.into(), output: "stopped by the orchestrator".into() }
}

/// The last `cap` bytes of `s`, cut on a char boundary, with a note of what was dropped.
fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {} bytes elided ...]\n{}", start, &s[start..])
}

/// Run `git` at `at`; stdout trimmed on success, stderr trimmed on failure.
fn git(at: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(at)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{GitWorktreeWorkspace, Workspace};
    use std::time::Instant;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "symphony-gate-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sh_git(at: &Path, args: &[&str]) -> String {
        match git(at, args) {
            Ok(s) => s,
            Err(e) => panic!("git {args:?} failed: {e}"),
        }
    }

    /// A repo on `master` with one commit, plus a worktree for `iss-1` branched from it.
    fn repo_and_worktree(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = tmp(tag);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh_git(&repo, &["init", "-q", "-b", "master"]);
        sh_git(&repo, &["config", "user.email", "test@example.com"]);
        sh_git(&repo, &["config", "user.name", "test"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        sh_git(&repo, &["add", "base.txt"]);
        sh_git(&repo, &["commit", "-q", "-m", "init"]);
        let repo = repo.canonicalize().unwrap();

        let ws = GitWorktreeWorkspace::new(dir.join("workspaces"), &repo).unwrap();
        let wt = ws.prepare("iss-1", "MT-1").unwrap().path;
        (dir, repo, wt)
    }

    fn commit(at: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(at.join(file), content).unwrap();
        sh_git(at, &["add", file]);
        sh_git(at, &["commit", "-q", "-m", msg]);
    }

    fn issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "t".into(),
            body: None,
            state: "In Progress".into(),
            priority: None,
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    fn wait(h: &Arc<dyn GateHandle>) -> Verdict {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(v) = h.finished() {
                return v;
            }
            assert!(Instant::now() < deadline, "gate did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// The guard for issue #21 in one test: `master` moves after the fork, the branch carries a
    /// commit, and the gate command can only pass on a tree that has master's new file. Drop
    /// the rebase and the command fails; skip the command and the assertion on its side effect
    /// fails; run the command before the rebase and it fails too.
    #[test]
    fn the_branch_is_rebased_onto_the_base_before_the_gate_runs_on_the_rebased_tree() {
        let (dir, repo, wt) = repo_and_worktree("rebase-then-gate");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(
            &repo,
            Some("master".into()),
            vec![
                argv(&["git", "cat-file", "-e", "HEAD:from_master.txt"]),
                argv(&["touch", "gate-ran"]),
            ],
        );
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { rebased: true });
        assert!(wt.join("from_master.txt").exists(), "the worktree must now sit on master's tip");
        assert!(wt.join("agent.txt").exists(), "with the agent's commit replayed on top");
        assert!(wt.join("gate-ran").exists(), "and the gate must actually have run");
        let master = sh_git(&repo, &["rev-parse", "master"]);
        assert!(
            git(&wt, &["merge-base", "--is-ancestor", &master, "HEAD"]).is_ok(),
            "master must be an ancestor of the rebased branch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths() {
        let (dir, repo, wt) = repo_and_worktree("conflict");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        let before = sh_git(&wt, &["rev-parse", "HEAD"]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Conflict { paths: vec!["base.txt".into()] });
        assert_eq!(
            sh_git(&wt, &["rev-parse", "HEAD"]),
            before,
            "the branch must be left as the agent made it"
        );
        assert!(
            git(&wt, &["rev-parse", "--verify", "--quiet", "REBASE_HEAD"]).is_err(),
            "no rebase may be left in progress"
        );
        assert!(!wt.join("gate-ran").exists(), "a conflict stops everything downstream");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_reports_its_output_and_stops_the_sequence() {
        let (dir, repo, wt) = repo_and_worktree("failing-command");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(
            &repo,
            Some("master".into()),
            vec![
                argv(&["git", "rev-parse", "--verify", "no-such-ref-anywhere"]),
                argv(&["touch", "second-ran"]),
            ],
        );
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, output } => {
                assert_eq!(step, "git rev-parse --verify no-such-ref-anywhere");
                assert!(
                    output.contains("fatal"),
                    "git's own stderr must reach the agent, got: {output}"
                );
                assert!(output.contains("exited with"), "and so must the exit status: {output}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!wt.join("second-ran").exists(), "the first failure ends the gate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_branch_with_nothing_beyond_the_base_is_not_gated() {
        let (dir, repo, wt) = repo_and_worktree("no-commits");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::NoCommits);
        assert!(!wt.join("gate-ran").exists(), "there is nothing to hand off, so nothing to check");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dirty_worktree_is_a_failure_the_agent_can_fix_not_a_conflict() {
        let (dir, repo, wt) = repo_and_worktree("dirty");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");
        std::fs::write(wt.join("agent.txt"), "uncommitted edit\n").unwrap();

        let gate = GitGate::new(&repo, Some("master".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, .. } => assert_eq!(step, "rebase onto master"),
            other => panic!("expected Failed at the rebase, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_base_fails_rather_than_rebasing_onto_nothing() {
        let (dir, repo, wt) = repo_and_worktree("bad-base");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(&repo, Some("no-such-branch".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert!(
            matches!(&verdict, Verdict::Failed { step, .. } if step.starts_with("resolve base")),
            "got {verdict:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_base_means_the_repositorys_own_head() {
        let (dir, repo, wt) = repo_and_worktree("head-base");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(&repo, None, vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { rebased: true });
        assert!(wt.join("from_master.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn killing_a_running_gate_stops_its_command_and_reports_a_failure() {
        let (dir, repo, wt) = repo_and_worktree("kill");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["sleep", "60"])]);
        let h = gate.start(&issue(), &wt);
        let deadline = Instant::now() + Duration::from_secs(30);
        while h.step() != "sleep 60" {
            assert!(Instant::now() < deadline, "the gate never reached its command");
            std::thread::sleep(Duration::from_millis(10));
        }

        let started = Instant::now();
        let result = h.kill(5_000);
        assert!(started.elapsed() < Duration::from_secs(10), "kill must not wait the sleep out");
        assert!(matches!(result, KillResult::Stopped | KillResult::Forced), "got {result:?}");
        assert!(matches!(h.finished(), Some(Verdict::Failed { .. })), "a killed gate did not pass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_output_keeps_its_tail_where_the_summary_is() {
        let long = format!("{}\nTHE END", "x".repeat(OUTPUT_CAP * 2));
        let t = tail(&long, OUTPUT_CAP);
        assert!(t.ends_with("THE END"));
        assert!(t.starts_with("[... "), "must say what was dropped");
        assert!(t.len() < OUTPUT_CAP + 64);
    }
}
