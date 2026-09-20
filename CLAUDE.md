# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`symphony-cc` is a tracker-driven orchestrator for Claude Code agents: a daemon that polls an
issue tracker, opens a workspace per issue, runs a coding-agent session in it, and reconciles
what comes back. It is a Rust reimplementation of the coordination layer described in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`, written after a review that
found several concrete defects in that design.

A lot of this code exists specifically in order *not* to have those defects. Read
**Invariants** before changing anything in `src/sched/`.

Slices 1–2 are complete and green: a deterministic core with a fake behind every external
seam, then real git worktrees and a real `~/.claude/tasks` projection behind those same
seams. A real tracker and real workers are the open issues.

## Commands

```bash
cargo test                                 # 57 unit + 20 integration
cargo test --lib                           # unit only
cargo test --test scheduler                # integration only
cargo test a_permanent_failure             # one test; the arg is a substring match

cargo clippy --all-targets -- -D warnings  # the standing bar is zero warnings
cargo fmt --check

cargo run -- --tui                         # dashboard against the fake tracker
cargo run -- --max-ticks 20                # headless smoke run, then exit
cargo run --example dashboard_preview      # render the UI to stdout, no terminal needed
```

`SYMPHONY_DB=/tmp/x.db` points the store somewhere disposable — worth doing before any run
that might write state you do not want kept. `SYMPHONY_TASKS_ROOT=/tmp/tasks` does the same
for the `~/.claude/tasks` projection, so a smoke run's demo issues (`iss-001`, `MT-601`, ...)
don't land in your real Claude Code task list. `RUST_LOG=symphony_cc=debug` raises the log
level; logs always go to stderr, because under `--tui` the alternate screen owns stdout.

Headless runs now create real `git worktree`s under `workspace.root` (default
`.symphony/workspaces`, gitignored) against `workspace.repo` (default `.`) and real files
under `~/.claude/tasks/<derived-session-id>/`. Both are best-effort seams — a worktree or
projection failure degrades the run, it does not stop it — but they are real disk and git
state, not a simulation, so use the env overrides above when you just want to watch the
scheduler and don't want the side effects.

`dashboard_preview` is the fastest way to see a layout change: it renders a canned `Snapshot`
through ratatui's `TestBackend`, so there is no terminal and no scheduler involved.

## Architecture

**The scheduler is the only authority.** Everything external sits behind a trait, and each
trait has a fake: `Clock`, `Tracker`, `Worker`, `Workspace`, `Store`, `Projector`. That is
what lets [tests/scheduler.rs](tests/scheduler.rs) drive real `Scheduler::tick()` calls with
no sleeps and nothing to flake — time only moves when a test moves it.

**Tick order is load-bearing** ([src/sched/mod.rs](src/sched/mod.rs)):

```
harvest_finished → detect_stalls → refresh_running   ← unconditional
                 ↓
            cfg.preflight()                          ← gate: on failure, return here
                 ↓
dispatch_due_retries → dispatch_new → publish
```

Reconciliation runs before the gate so that a broken config stops *new* dispatch without also
stranding the runs already in flight. Do not move the `preflight()` call earlier.

**Four rules that bind everywhere:**

1. The clock is injected. Nothing outside [src/clock.rs](src/clock.rs) may call
   `Instant::now` or `SystemTime::now`. Monotonic (`Mono`) for every interval — stall, backoff
   — so an NTP step cannot fire them early; wall (`Wall`) only for display and for
   `retry.due_at`, which has to survive a restart.
2. The claim commits before the worker exists: `ensure → claim → prepare → spawn`, in that
   order, in `launch()`. Spawning first leaves a window where a fast-exiting worker reports
   against state that was never written.
3. The TUI never reads the store. The scheduler publishes an immutable `Snapshot` over a
   `tokio::sync::watch` channel and the UI renders that and nothing else. Headless is the
   default and `--tui` opts in, which is what keeps the dashboard from becoming load-bearing.
4. The projection is one-way ([src/project.rs](src/project.rs)). The orchestrator writes to
   `~/.claude/tasks` and never reads it back for a scheduling decision — it is Claude Code's
   internal store with no published schema, so a change there must cost a dashboard, not the
   scheduler. `TasksProjector` probes one existing task file's shape at startup and disables
   itself with a warning if the keys it depends on are missing; that probe is the one read this
   type performs, and its result only ever flips this type's own on/off switch.

**The store is a cache of judgment, not a system of record.** Losing `symphony.db` degrades
to stateless re-polling, never to incorrect behaviour.

**Worth knowing:** reconciliation lives in `sched/mod.rs` rather than its own module — it
mutates the same `running` map as dispatch, so splitting it meant threading the whole
scheduler through a free function. The `Tracker` trait is deliberately a two-method read
kernel (`by_states`, `by_ids`); ticket *mutations* belong to the agent through host-executed
tools, not to this trait. `Workspace` has two implementations behind the trait:
`DirWorkspace` (plain directories, what the scheduler tests use — real git is slower and adds
nothing to a test that fakes the worker too) and `GitWorktreeWorkspace` (real, what `main.rs`
wires by default). Reuse in the latter checks for a `.git` *file* at the target path, not
bare existence — a plain directory there, e.g. left by a prior `DirWorkspace` run against the
same root, must surface through git's own "already exists" error rather than being silently
trusted as an already-prepared worktree.

## Invariants

Each of these closes a defect found in the original spec, and each has a test that fails
without it. Several only fail in the exact scenario they were written for, so a regression
here can pass a casual `cargo test` reading — check the named test is still meaningful, not
just still green.

| Invariant | Mechanism | Guard test |
|---|---|---|
| Backoff cannot overflow or collapse | cap the *exponent* (`EXP_CAP = 16`), not just the product | `backoff_never_overflows_or_collapses_at_any_attempt_count` |
| No 1s continuation respawn loop | explicit `Outcome` verdict + escalating delay + `max_turns_per_issue` | `continuation_backs_off_instead_of_respawning_every_second` |
| A finished issue is not re-dispatched | `parked_state`, cleared only when the ticket actually moves | `a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged` |
| Permanent failures stop | `ErrorClass::retryable()` → immediate quarantine | `a_permanent_failure_quarantines_immediately_rather_than_retrying_forever` |
| No workspace is deleted under a live agent | `kill(grace)` blocks until confirmed stopped, *then* `remove` | `a_ticket_moving_to_terminal_stops_the_run_and_cleans_up` |
| One tracker blip cannot kill a run | `refresh_miss_grace`, reset on reappearance | `one_invisible_refresh_is_survivable_but_two_are_not` |
| A workspace path cannot escape its root | `guard()` on **both** `prepare` and `remove` | `hostile_identifiers_stay_inside_the_root` |

Three of these — the verdict, the per-issue turn budget and `parked_state` — are independent
brakes on the same runaway. Removing any one of them looks safe because the other two still
hold. They cover different paths; keep all three.

## Conventions

- **Test names are sentences asserting the invariant**, not `test_foo`. If you cannot name
  what a test defends, it probably is not defending anything.
- **Comments carry the why**, usually which failure mode is being avoided. The what is in the
  code. Match the surrounding density — this codebase comments decisions, not lines.
- `max_width = 100`, `use_small_heuristics = "Max"` ([rustfmt.toml](rustfmt.toml)). Run
  `cargo fmt` rather than hand-wrapping; it makes different choices than you will.
- New external effects get a trait and a fake in the same commit, or the scheduler tests stop
  being able to reach the new code path.
- Rust edition 2024 — let-chains (`if x && let Some(y) = z`) are available and used.

## Dogfooding

The backlog is GitHub Issues on this repository, labelled `agent` — which is exactly the shape
`TrackerConfig.required_labels` filters on. Work is picked up from there, not from a plan file.

The worktree and projection slices have landed: `main.rs` wires `GitWorktreeWorkspace`
(`workspace.repo`, defaulting to `.`) and `TasksProjector` by default, so a headless run
against this repo creates real worktrees under `.symphony/workspaces` and real files under
`~/.claude/tasks`. A real tracker and real workers are what remain before `symphony-cc` can
poll this repository and dispatch against its own backlog end to end. Until the tracker
lands, `symphony.toml` stays on `kind = "fake"`. When you change scheduler behaviour, ask
whether the change would still be correct when the agent running it is working on this repo.

[.mcp.json](.mcp.json) hands the agent rust-analyzer over MCP, so navigation in this repo is
LSP rather than grep — which is what makes the invariant table above checkable: whether
`guard_within` is still reached from both `prepare` and `remove` is a find-references
question. It needs `rustup component add rust-analyzer rust-src` plus
`cargo install rust-analyzer-mcp` on the host. Being committed, it is inherited by every
worktree under `.symphony/workspaces`, so each dispatched agent indexes its own copy of the
tree. That is intended, but it is not free — budget roughly 1-2 GB resident and one
`cargo check` per concurrent run when setting `agent.max_concurrent`.

One trap, and it bites exactly the use above: `references`, `definition` and `hover` answer
from whatever is indexed *so far* rather than waiting, so during the first load they come
back empty — and empty is indistinguishable from *no callers*, which reads as an invariant
that has already been broken. Only `rename` (which refuses outright) and `diagnostics`
(which carries a `complete` flag) wait for a quiescent workspace. Ask again until an answer
is non-empty before concluding anything from one.

## Constraints for the worker and broker slices

Decisions already taken that are expensive to rediscover:

- Build the worker's child environment from an explicit **allowlist**. Do not inherit and
  scrub — a denylist is fragile, and one missed variable leaks a tracker credential into a
  coding agent.
- **Never launch a worker via `bash -lc`.** A login shell re-imports from the operator's
  dotfiles exactly the secrets that were just scrubbed.
- The MCP tool broker executes tracker writes host-side while holding the credential. The
  worker receives results, never a raw token.

## Troubleshooting

If linking fails, or `git` refuses every invocation with *"You have not agreed to the Xcode
license agreements"*, the problem is which developer directory is active — not your code, and
not a missing toolchain:

```bash
xcode-select -p    # pointing at /Applications/Xcode.app means an unaccepted license
sudo xcode-select --switch /Library/Developer/CommandLineTools
```

`xcode-select --install` does **not** fix this. It installs the Command Line Tools; it does
not make them active, so the reported symptom is unchanged and the real cause stays hidden.
