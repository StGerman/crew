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

Slices 1–6 are complete and green: a deterministic core with a fake behind every external
seam, then real git worktrees and a real `~/.claude/tasks` projection, then a real GitHub
Issues tracker, then a real `claude -p` worker, then a host-side MCP tool broker that lets the
agent write to its own ticket without ever holding the credential, and an HTTP ops API over
the published snapshot with a `status` client in front of it.

## Commands

```bash
cargo test                                 # 178 unit + 63 integration
cargo test --lib                           # unit only
cargo test --test scheduler                # scheduler integration only
cargo test --test api                      # ops API integration only
cargo test a_permanent_failure             # one test; the arg is a substring match

cargo clippy --all-targets -- -D warnings  # the standing bar is zero warnings
cargo fmt --check

cargo run -- --tui                         # dashboard against the fake tracker
cargo run -- --max-ticks 20                # headless smoke run, then exit
cargo run -- --api 127.0.0.1:8787          # headless, with the ops API on for this run
cargo run -- status                        # what a running daemon is doing, read over that API
cargo run -- status MT-649                 # one issue in full: phase, attempt, turns, cost, branch
cargo run --example dashboard_preview      # render the UI to stdout, no terminal needed
cargo run --example broker_live            # real `claude` against a real broker; spends tokens
```

The first three are the commit gate, and [.github/workflows/ci.yml](.github/workflows/ci.yml)
now runs them on every push and pull request rather than trusting whoever remembers — which is
the only version that survives a dispatched agent leaving a branch behind. `rust-toolchain.toml`
pins the compiler so CI, this machine and every worktree agree on what "it compiles" means, and
CI builds `--locked` so a drifted `Cargo.lock` fails rather than being quietly rewritten. One
thing to preserve if you edit that workflow: it must never *run* an example. `broker_live`
spawns a real `claude` and spends tokens, and only a Cargo default (examples are `test = false`)
keeps `cargo test` from calling it — the workflow says so at the top.

`SYMPHONY_DB=/tmp/x.db` points the store somewhere disposable — worth doing before any run
that might write state you do not want kept. `SYMPHONY_TASKS_ROOT=/tmp/tasks` does the same
for the `~/.claude/tasks` projection, so a smoke run's demo issues (`iss-001`, `MT-601`, ...)
don't land in your real Claude Code task list. `RUST_LOG=symphony_cc=debug` raises the log
level; logs always go to stderr, because under `--tui` the alternate screen owns stdout.

Every run's raw event stream lands under `<workspace.root>/.transcripts/`, one `.jsonl` per
run, and the `dispatched` log line names the file. That is the first thing to reach for when
asked what a run actually did — `jq -c 'select(.type=="assistant")' <file>` for the turns,
`grep symphony_run_end` for how it exited. Tune or switch it off under `[transcripts]`; the
bounds there are what make it safe to leave on.

Headless runs now create real `git worktree`s under `workspace.root` (default
`.symphony/workspaces`, gitignored) against `workspace.repo` (default `.`) and real files
under `~/.claude/tasks/<derived-session-id>/`. Both are best-effort seams — a worktree or
projection failure degrades the run, it does not stop it — but they are real disk and git
state, not a simulation, so use the env overrides above when you just want to watch the
scheduler and don't want the side effects.

One thing the overrides do not cover: running the daemon from *inside* a worktree — which is
what a dispatched agent's cwd is. `GitWorktreeWorkspace::new` refuses that at startup, because
`workspace.repo = "."` there is a linked worktree and the worktrees a run would create register
in the top-level checkout's shared `.git`, where the orchestrator owning it never recorded them
(#29). The error names the way out: point `workspace.repo` at a throwaway clone and
`workspace.root` beside it. Setting `SYMPHONY_DB` alone does not help — the litter was never
in the store.

```bash
GITHUB_TOKEN=$(gh auth token) cargo run -- --config symphony.github.toml --max-ticks 3
```

points the tracker at this repo's own real Issues instead of the fake demo data —
`symphony.github.toml` is checked in and ready to use, no token in it. Today that lists this
repo's five open, `agent`-labelled issues and dispatches none of them, because none has an
assignee yet (see `src/tracker/github.rs`'s module doc for the dispatchability rule and the
state-label convention).

`worker.kind = "claude"` is the other half — and it is a separate switch from the tracker on
purpose (see `WorkerConfig`'s doc in [src/config.rs](src/config.rs)): a real tracker with the
fake worker is a safe way to watch real dispatch decisions without spawning real agents,
turning "point this at a real repo" into "start editing that repo" only when both are flipped
deliberately. With it on, `cargo run` spawns real `claude -p` processes with
`--permission-mode bypassPermissions` — no human answers a tool-use prompt in a headless
dispatch — against real git worktrees. Treat `--max-ticks` on a config with `worker.kind =
"claude"` as spawning real, tool-using agent processes, not a dry run.

`broker_live` is the counterpart for the tool broker, and it exists because nothing inside
this crate can prove the real CLI agrees with it: the transport tests drive a socket this crate
also wrote, and the tool tests drive a fake tracker. It spawns an actual `claude` process
against an actual broker and asserts the orchestrator performed exactly one write. It needs a
working login and spends tokens; it writes to no tracker.

`dashboard_preview` is the fastest way to see a layout change: it renders a canned `Snapshot`
through ratatui's `TestBackend`, so there is no terminal and no scheduler involved.

## Architecture

**The scheduler is the only authority.** Everything external sits behind a trait, and each
trait has a fake: `Clock`, `Tracker`, `Worker`, `Workspace`, `Store`, `Projector`. That is
what lets [tests/scheduler.rs](tests/scheduler.rs) drive real `Scheduler::tick()` calls with
no sleeps and nothing to flake — time only moves when a test moves it.

**Tick order is load-bearing** ([src/sched/mod.rs](src/sched/mod.rs)):

```
recover()                                        ← first tick only
                 ↓
harvest_finished → observe_progress → harvest_gates → detect_stalls → refresh_running
                 ↓
            cfg.preflight()                          ← gate: on failure, return here
                 ↓
sweep_parked → dispatch_due_retries → dispatch_new → publish
```

Reconciliation runs before the gate so that a broken config stops *new* dispatch without also
stranding the runs already in flight. Do not move the `preflight()` call earlier. `sweep_parked`
is the one reconciliation step deliberately *behind* it: it deletes workspaces on the strength
of `is_terminal`, and an active/terminal overlap — one of the things the gate rejects — is
exactly what would make it delete the workspace of an issue about to be dispatched. It also
runs on its own cadence (`agent.parked_sweep_interval_ms`, default 5 min) rather than every
tick, because parked issues are not urgent and each sweep is one `by_ids` read per issue still
parked.

`harvest_gates` is where a `Done` becomes a verdict. With a handoff gate attached (see below), a
run whose agent reports `Done` leaves `running` for a `gating` map instead of being released: its
claim stays held, its run row stays open, and the gate — rebase onto the base, then the configured
commands, in the run's own worktree — runs on a thread the scheduler polls. This step turns the
gate's answer into `Done`, `Blocked` or `Continue` and hands it to the same `apply_outcome` an
agent's verdict goes through, which is what makes a gate-sent continuation subject to the same
turn budget and the same escalating delay. It sits with the rest of reconciliation, ahead of
`preflight`, because a claim held mid-rebase must not stay held behind a config typo.

`recover()` is startup reconciliation, and it runs ahead of the gate for the same reason: a
claim stranded by the last process must not stay stranded behind a config typo. It lives inside
`tick()` rather than in `main.rs` on purpose — recovery a second entry point can forget to call
is recovery that silently does not happen, which is the exact failure it exists to fix. It is
callable directly (`Scheduler::recover`) and idempotent, so a caller that wants it eagerly can
have it.

**Four rules that bind everywhere:**

1. The clock is injected. Nothing outside [src/clock.rs](src/clock.rs) may call
   `Instant::now` or `SystemTime::now`. Monotonic (`Mono`) for every interval — stall, backoff
   — so an NTP step cannot fire them early; wall (`Wall`) only for display and for
   `retry.due_at`, which has to survive a restart.
2. The claim commits before the worker exists: `ensure → claim → prepare → spawn`, in that
   order, in `launch()`. Spawning first leaves a window where a fast-exiting worker reports
   against state that was never written.
3. No observer reads the store. The scheduler publishes an immutable `Snapshot` over a
   `tokio::sync::watch` channel, and the TUI, the HTTP API and `symphony-cc status` render that
   and nothing else. Headless is the default and `--tui` opts in, which is what keeps the
   dashboard from becoming load-bearing. The rule cuts both ways: an observer that needs
   something the snapshot does not carry does not get a `Store`, it gets a new field on
   `Snapshot` — which is why run history lives on `Row`, and why `Row.branch` was added when
   the status client needed it rather than having the client ask git.
4. The projection is one-way ([src/project.rs](src/project.rs)). The orchestrator writes to
   `~/.claude/tasks` and never reads it back for a scheduling decision — it is Claude Code's
   internal store with no published schema, so a change there must cost a dashboard, not the
   scheduler. `TasksProjector` probes one existing task file's shape at startup and disables
   itself with a warning if the keys it depends on are missing; that probe is the one read this
   type performs, and its result only ever flips this type's own on/off switch.

**The store is a cache of judgment, not a system of record.** Losing `symphony.db` degrades
to stateless re-polling, never to incorrect behaviour — the session id lives there too, so
losing it costs cold continuations rather than a wrong conversation. The claim is the one entry
that could invert that, because *keeping* it across a hard kill is what went wrong: an issue
marked `running` with nothing running is refused by `claim()` forever, is invisible to
`detect_stalls`, and has no retry row to bring it back. `Scheduler::recover` is what holds the
contract — it releases those claims at startup and reconciles their worktrees, so the worst a
surviving database can do is still cost a re-poll.

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
trusted as an already-prepared worktree. The branch, not the directory, is what a run leaves
behind: `remove` deletes the worktree but only deletes the branch when git's own merged check
says it carries nothing `repo`'s HEAD does not already have, and `prepare` attaches to an
existing branch that does carry commits rather than `-B`-resetting it. Cleanup is triggered by
a ticket reaching a terminal state, and closing a ticket is not a decision to throw away the
work done under it. That same merged check is what makes nesting expensive: an orchestrator
started inside another run's worktree (the agent for #24 did, to exercise the API) creates
worktrees whose branches sit on the *parent's* commit and so are never merged into `master`,
and `branch -d` keeps every one of them. `new` therefore refuses when `repo` or `root`
resolves inside a linked worktree of the repository — refused rather than redirected to the
top-level root, because redirecting would still leave registrations and branches the owning
orchestrator does not know about — and `remove` reconciles what earlier binaries left: it
collects the worktrees registered beneath the path before `worktree remove --force` deletes
their directories, prunes the stale registrations (first — `-d` refuses a branch a registered
worktree still pins), then gives each nested branch the same `-d` the parent's own gets. A
nested branch carrying commits is kept and named in a `warn` log line, with what to run once
its parent is merged; the merged check is not weakened for litter. `Prepared.branch` reports that name upwards so it reaches the dispatch log, and
`Workspace::branch_for` — pure naming, like `path_for` — answers the same question for the
snapshot. Naming rather than probing is what lets a *finished* run still report its branch,
which is when a reviewer wants it; asking git per row per tick would put a subprocess on the
snapshot path. `Row.branch` is `None` until an issue has been dispatched at least once, because
before that the name is a prediction and pointing an operator at a ref nobody wrote is worse
than saying nothing.

`Tracker` gets its third implementation in [src/tracker/github.rs](src/tracker/github.rs):
`GithubTracker<H: Http>`, generic over a small `Http` seam (`FakeHttp` in tests, `UreqHttp` —
over `ureq` with `rustls`, no C toolchain needed — in `main.rs`). GitHub has no workflow
states beyond open/closed; the module doc there is the write-up of that mapping (a
`state:<name>` label convention) and should be read before touching it. Two contract details
worth knowing before changing either `Tracker` impl: `by_ids` must fail the whole call on
anything other than a clean 404 — a transport error silently dropped from the result would be
indistinguishable from the id having genuinely disappeared, which is exactly the ambiguity
`refresh_miss_grace` exists to bound, and bounding it needs the *real* miss count, not one
deflated by swallowed errors. And `ureq`'s default turns a non-2xx response into an `Err` that
discards the headers and body this adapter classifies on (rate-limit header, error message) —
`UreqHttp::default()` disables that (`http_status_as_error(false)`) so every status code
arrives as an ordinary response. `FakeHttp`-based tests are structurally blind to that class
of bug — they hand `GithubTracker` an already-correct `HttpResponse` — which is why
`ureq_http_tests` in the same file talks to a raw `TcpListener` instead.

A tracker failure has no issue to quarantine against — `by_states`/`by_ids` are batch calls,
not scoped to one ticket — so `TrackerError::class()` ([src/tracker/mod.rs](src/tracker/mod.rs))
reuses `ErrorClass::retryable()` only to pick a log level: `dispatch_new`, `dispatch_due_retries`
and `refresh_running` all skip the tick and try again either way, but a bad credential now logs
at `error` with a "will not resolve on its own" hint instead of blending into the same `warn` a
rate limit gets. That is the honest version of "an auth failure stops trying and gets loud" at
this scope; a literal per-issue quarantine here would be quarantining tickets a bad token had
nothing to do with.

`Worker` gets its real implementation in [src/worker/claude.rs](src/worker/claude.rs):
`ClaudeWorker`, over `claude -p --output-format stream-json`. Two things there were confirmed
against a real install rather than assumed, because guessing wrong would have meant a worker
that silently never worked: there is no `--max-turns` flag, so the per-session turn budget is
self-enforced — the reader thread counts `assistant` events and sends `SIGTERM` once the count
reaches `max_turns_per_session`, reporting `Outcome::Continue` itself; and `--bare` needs
`ANTHROPIC_API_KEY`, which an OAuth-authenticated operator (this dev machine included) does not
have, so it is not passed by default — the worker inherits whatever hooks and MCP servers the
operator's own `claude` config has until a dedicated API key changes that trade-off. `Outcome`
beyond done/failed — `Continue`, `Blocked` — has no structural signal from the CLI to key off,
so the worker's prompt asks the agent to end its final message with `SYMPHONY_OUTCOME:
continue: <reason>` or `SYMPHONY_OUTCOME: blocked: <reason>`; the module doc has the reasoning,
and it is a soft convention by design — an agent that forgets it just reads as `Done`.

Token totals come from the terminal `result` event and nowhere else (`Progress::tokens`, an
`Option`). The first live dispatch (#7) summed the `usage` block of every streamed `assistant`
event instead and recorded ten million input tokens and four hundred output tokens over 83
turns: the CLI emits one `assistant` event per content block, each carrying the whole turn's
usage, and the per-event `output_tokens` is a streaming placeholder. Two things follow. Stall
detection no longer has token counters to watch, so `Progress::events` — a count of every
parsed stream event, tool results included — is the liveness signal, and it is a better one: an
agent an hour into a long tool call was previously indistinguishable from a silent one. And a
run that ends without a `result` — killed, crashed, or cut off by the turn budget, which on a
real install produces no `result` at all — reports `None`, stored as NULL, and lands in the
dashboard's `(+N uncounted)` tally rather than as a zero or an estimate. Schema v3 dropped the
totals recorded before this; they were unrelated to the real cost, not a rough version of it.

**Every run leaves a transcript** ([src/transcript.rs](src/transcript.rs)). The reader copies
each `stream-json` line to a per-run file *before* deciding whether the parser has a use for it
— so the `system`, `rate_limit_event` and tool-call lines it drops, and the lines it could not
parse at all, are still there afterwards — then appends how the process exited and what it said
on stderr. The path is on the run row (`Store::run`, `runs_for`) and in the dispatch log line
and the TUI detail pane, so "show me what run X did" needs no knowledge of the layout. Three
things there are load-bearing and each looks removable: writes are **unbuffered, one per line**,
because a block-buffered transcript reproduces the exact defect that caused the wrong diagnosis
this exists to prevent; the root sits **beside** the worktrees rather than inside one, because a
worktree is a git checkout the agent commits from *and* is deleted when its ticket goes
terminal, which is the moment the transcript becomes worth reading; and `prune` is handed the
paths of runs still in `running`, because a **stalled** run stops writing by definition, so its
file ages past the newest `keep_runs` while the process behind it is still alive — pruning it
would lose the transcript of the run most likely to need one and leave the writer on an
orphaned inode. Best-effort like the projector: a transcript that cannot be opened costs a
post-mortem, never a dispatch.

`Tracker` stays a read kernel; the *write* half lives on a separate trait,
`TrackerWrites` ([src/broker/writes.rs](src/broker/writes.rs)), whose only caller is the broker
([src/broker/](src/broker/)). `GithubTracker` implements both over one credential that never
leaves the process. The broker hands each dispatched run an MCP server over loopback HTTP, with
a per-run bearer token in the URL path, wired in with `claude -p --mcp-config`. **No tool takes
an issue id** — the target is resolved from the token, which is the whole security property; a
call that passes one anyway is refused and audited rather than ignored, because a silent drop
would make the attempt indistinguishable from a well-formed call in exactly the log a reviewer
would read. Budgets are per-run *and* per-issue, and charge attempts rather than successes: a
per-run cap alone bounds nothing, because the continuation loop opens a fresh run each time —
the same gap `max_turns_per_issue` closes for turns — and a budget only spent on successes
would leave a loop of failing writes free. A session is an RAII guard held inside the run's own
record, so the token is revoked and its config file deleted on every path that ends a run,
including ones not yet written. A broker that cannot bind, or a session that cannot open,
degrades to an agent without tools and never to a failed dispatch.

The transport is hand-rolled rather than built on `rmcp`, and
[src/broker/server.rs](src/broker/server.rs)'s module doc is the write-up — the short version
is that `rmcp`'s streamable-HTTP server is a `tower::Service` with no listener, so it adds ~35
crates and still needs axum on top, and it is async where `Tracker`, `TrackerWrites` and
`Scheduler::tick` are all deliberately blocking. What it would wrap is four methods. The wire
details there were recorded from a live `claude 2.1.278` handshake, not read off a spec: the
first request is `server/discover` (answer `-32601` and the client falls through), the
initialized notification must get `202` with no body, and the SSE `GET` can be refused with
`405` — which is what makes a plain JSON response to each POST the entire transport.

A continuation resumes rather than restarts: the scheduler names the conversation with
`--session-id` before the first attempt and passes `--resume <id>` for every attempt after, so
the turn budget is not spent twice over on the same re-orientation. The name is written to the
store before the process exists, for the same reason the claim is — the child cannot be what
records it. A run that takes no turns drops the name, which is how a session the CLI no longer
holds degrades to a cold start instead of failing every retry identically into quarantine.

The first live end-to-end run of `ClaudeWorker` (real agent, real worktree, cut off mid-run by
`--max-ticks`) left an orphaned `claude` process running after `cargo run` had already
returned. `RunHandle` says plainly that dropping a handle does not stop the work, and nothing
before this had ever exercised the "exit with a run still in flight" path — the scheduler's own
tick loop calls `harvest_finished`/`terminate` for every state transition except "the process
just quit." `Scheduler::shutdown()` (called from `main.rs` after the loop) and a `Drop for
Scheduler` safety net (for a panic or an early `?` return that skips the explicit call) both
terminate every run still in `self.running` before letting the process end. If you add another
place `main.rs` can exit, check that this still runs.

**A `Done` is a claim until the handoff gate agrees** ([src/gate/](src/gate/)). Issue #21: two
branches dispatched off the same base each passed `cargo test` alone, and their combination did
not compile — three defects that existed only in the merge, which neither agent could have seen.
Nothing rebased a finished branch onto the current base, and nothing re-ran the checks after. So
`Gate` is the seam between an agent saying `Done` and a human being handed the branch: `GitGate`
resolves `gate.base` in `workspace.repo` (not in the worktree, whose HEAD is the run's own
branch), skips a branch with no commits beyond it, rebases, and on a clean rebase execs each
`gate.commands` argv directly in the worktree — no shell, for the same reason the worker has
none, and this one inherits the operator's environment because it is the operator's own suite
with no agent involved. The verdict is deliberately three-way and the split is the point: a
**conflict** is a human's problem, so the rebase is aborted (the branch goes back to exactly what
the agent committed, which is what keeps `remove`'s merged check on its side) and the issue parks
`Blocked` naming the paths; a **failing command** is the agent's, so the verdict is `Continue` and
the output rides into the next spawn as the `brief` parameter on `Worker::spawn`, where the real
worker puts it in the prompt; and `gate.max_failures` **consecutive** failures escalate to
`Blocked`, because a gate that can be failed forever is the continuation runaway wearing a new
name. A `Blocked` reason now also lands in the store's `last_error`, so the dashboard shows why
an issue is parked instead of only the log. Setting no gate is a decision, not a degrade — unlike
the broker or the projector, a scheduler without one hands a `Done` to a human exactly as the
agent left it, so `main.rs` attaches one whenever `gate.enabled` is true (the default, with an
empty command list, which makes the default a rebase and nothing more) and the scheduler tests
attach `FakeGate` explicitly. `symphony.github.toml` sets the three commands from **Commands**
above; the fake worker never commits, so under `symphony.toml` every gate finds nothing to hand
off.

The ops API ([src/api/mod.rs](src/api/mod.rs)) is the second observer of that same published
snapshot: `GET /api/v1/snapshot`, `GET /api/v1/issues/:identifier`, `POST /api/v1/refresh`,
`POST /api/v1/unquarantine/:identifier`. `Api` holds a `watch::Receiver` and a command sender
and no `Store`, so rule 3 is enforced by the type rather than by discipline — and the two
`POST`s can express nothing the dashboard's `r` and `u` keys cannot. Off by default (`[api]
enabled`, or `--api <addr>` for one run) and loopback unless `api.allow_public` says otherwise,
because those two routes control agent execution. Deliberately *not* validated in
`Config::preflight`: preflight gates dispatch, so a typo in an address the scheduler never uses
must not be what stops it — `api::bind` parses it once, and a failure there is logged and
costs the API alone. The write path goes `HTTP task → Command → the loop in main.rs → oneshot`,
which is what keeps a hung client off the tick: the scheduler answers into a channel whose
receiver may already be gone and never waits to find out. The HTTP is hand-rolled (~200 lines,
no keep-alive, one response type) for the same reason the rest of this crate is small; the
tests in [tests/api.rs](tests/api.rs) drive it over a real loopback socket against a real
`Scheduler`, because framing and connection-close bugs are exactly what a fake would hide.

Two notes for anyone adding an endpoint. `:identifier` resolves the dispatch id first and an
identifier second, and answers `409` with the candidate ids when one identifier names two
issues — identifiers are not unique, which is the same fact `worktree_key` exists for. And
`POST /unquarantine` on an issue that is not quarantined is a `200` saying so, not an error:
the guard lives in `Store::unquarantine`'s `WHERE` clause, because an unconditional version
would reset a *running* issue's phase to `released` and let the next tick dispatch a second
agent onto its worktree.

`symphony-cc status` ([src/api/client.rs](src/api/client.rs), rendered by
[src/api/render.rs](src/api/render.rs)) is the other end, and it exists because the API on its
own was not enough: it had been able to answer for hours at the moment diagnosis instead went
to a block-buffered log file and got the wrong answer (#24). Nothing was missing server-side —
what was missing was something to type. So it is a client and nothing else. `run_status`
returns before any of `main`'s setup, holding no `Store`, no worktree and no tracker
credential: an operator asking what is running must not be able to disturb it, and a second
process on `symphony.db` while the daemon holds it would be exactly that. It shares `Snapshot`
and `Row` with the server instead of re-describing them, so a renamed field fails the build
rather than rendering a blank column, and it reuses the dashboard's `fmt_count`/`fmt_ms`/
`Phase::label` so a duration means the same thing on all three surfaces.

Two behaviours there are load-bearing and easy to "simplify" away. It finds the daemon itself —
`--api`, then `[api] bind`, then `DEFAULT_API_BIND` — and reads the config *leniently* rather
than through `Config::load`, because that runs `preflight`, and preflight gates dispatch: an
unset `tracker.owner` is a real problem for the daemon and none at all for a client asking what
the daemon is doing. And `StatusError` keeps apart the two failures that a stack trace makes
look identical: nothing listening (no daemon — or one with its API off, which is the default
and so the likelier reading) versus a daemon that answered and refused. Collapsing those is
what has somebody restart a daemon that was never down, so each message names the address
tried, where that address came from, and the way out.

## Invariants

Each of these closes a defect found in the original spec. The later rows came instead from
dogfooding this orchestrator against its own backlog: the first review, the operator surface
built over the same state, issue #1's acceptance criterion made executable, the first live
dispatch and the review that followed it, the client put in front of that operator surface,
making a finished run diagnosable, a closed ticket whose worktree outlived it, an
orchestrator that ran inside its own worktree, and two branches that were each green
alone and broken together.

Every row has a test that fails without its mechanism. Several of those tests only fail in the
exact scenario they were written for, so a regression here can pass a casual `cargo test`
reading — check that the named test is still meaningful, not just still green.

| Invariant | Mechanism | Guard test |
|---|---|---|
| Backoff cannot overflow or collapse | cap the *exponent* (`EXP_CAP = 16`), not just the product | `backoff_never_overflows_or_collapses_at_any_attempt_count` |
| No 1s continuation respawn loop | explicit `Outcome` verdict + escalating delay + `max_turns_per_issue` | `continuation_backs_off_instead_of_respawning_every_second` |
| A finished issue is not re-dispatched | `parked_state`, cleared only when the ticket actually moves | `a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged` |
| Permanent failures stop | `ErrorClass::retryable()` → immediate quarantine | `a_permanent_failure_quarantines_immediately_rather_than_retrying_forever` |
| No workspace is deleted under a live agent | `kill(grace)` blocks until confirmed stopped, *then* `remove` | `a_ticket_moving_to_terminal_stops_the_run_and_cleans_up` |
| One tracker blip cannot kill a run | `refresh_miss_grace`, reset on reappearance | `one_invisible_refresh_is_survivable_but_two_are_not` |
| A workspace path cannot escape its root | `guard()` on **both** `prepare` and `remove` | `hostile_identifiers_stay_inside_the_root` |
| Cleanup cannot discard an agent's commits | `branch -d` (not `-D`) on remove; attach, not `-B`, on reuse | `a_branch_holding_committed_work_outlives_the_worktree_it_is_removed_with` |
| A dead session cannot strand an issue | drop the session name after a run with zero turns | `a_run_that_took_no_turns_is_not_retried_into_the_same_conversation` |
| A hard kill cannot strand a claim | startup `recover()`: a claim with no live run is stale, because `running` cannot cross a process boundary | `a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup` |
| A hard kill cannot zero an in-flight run's progress | `observe_progress` checkpoints `run.turns` once per tick when the count moved, and `close_open_runs` keeps it and charges it to `cumulative_turns` in one transaction | `a_run_interrupted_by_a_hard_kill_reports_its_last_known_turn_count_after_restart` |
| An agent cannot write to another ticket | no tool takes an issue id; the target comes from the per-run token | `a_call_naming_a_different_issue_is_refused_and_the_refusal_is_audited` |
| A looping agent cannot write without bound | per-run **and** per-issue budgets, charged on attempts not successes | `a_continuation_cannot_refresh_the_budget_by_opening_a_new_session` |
| A finished run keeps no write authority | the session is an RAII guard living in the `running` entry | `a_run_that_ends_takes_its_broker_authority_with_it` |
| Clearing a quarantine cannot release a live claim | `Store::unquarantine` is guarded on `quarantined_at IS NOT NULL` and reports what it did | `clearing_a_quarantine_that_is_not_there_does_not_release_a_live_claim` |
| A slow HTTP client cannot delay a tick | one task per connection, a `oneshot` reply the scheduler never waits on, and a bounded read timeout | `a_client_that_never_finishes_its_request_cannot_delay_a_tick` |
| The projection cannot become load-bearing | `publish` logs a projector error and returns `Ok`; nothing written is ever read back | `the_scheduler_makes_the_same_decisions_whether_the_projector_writes_fails_or_is_off` |
| A killed run cannot record a fabricated cost | totals are read only from the `result` event; a run that never emits one stores NULL, not a per-event sum | `a_run_that_dies_before_its_result_event_reports_no_token_total` |
| A half-applied migration cannot stop the store opening | each migration and the `user_version` bump that records it commit in one transaction | `a_migration_that_fails_partway_leaves_no_trace_and_does_not_advance_the_version` |
| "No daemon" is never confused with "daemon said no" | `StatusError` splits a refused connection from a refused request, and names the address and its source in both | `a_closed_port_reads_as_no_daemon_rather_than_a_refused_request` |
| The branch an operator is sent to is the one git checked out | `Workspace::branch_for` is the same naming function `prepare` uses, not a second spelling of it | `the_branch_the_snapshot_publishes_is_the_one_prepare_checks_out` |
| The published branch never names a ref that is gone or was never this run's | `Store::set_branch` persists what `prepare` returned and is cleared exactly when `Removed::branch_deleted` says cleanup deleted it — never recomputed from `identifier`, which `Store::ensure` can rename after dispatch | `the_published_branch_is_the_one_prepare_recorded_not_one_recomputed_from_the_current_identifier` |
| Retention cannot delete a live run's transcript | `prune` is handed the paths of runs still in `running` | `retention_bounds_the_transcript_directory_but_spares_a_stalled_runs_own_file` |
| A parked run's worktree is reclaimed once its ticket closes | `sweep_parked` re-reads parked ids on a bounded cadence, unparks what it cleans, and clears the published branch when cleanup deleted the ref | `a_parked_issue_that_is_later_closed_has_its_workspace_reclaimed_without_a_restart`, `sweeping_parked_issues_costs_tracker_traffic_bounded_by_the_interval_not_by_ticks`, `a_sweep_that_deletes_a_branch_clears_the_name_the_snapshot_publishes` |
| One orchestrator cannot nest its worktrees inside another's | `GitWorktreeWorkspace::new` refuses a `repo` or `root` inside a linked worktree of the repository; `remove` prunes registrations beneath the path it deletes, then `branch -d`s their branches with the merged check intact | `an_orchestrator_cannot_be_started_inside_another_runs_worktree`, `removing_a_worktree_reclaims_the_worktrees_nested_inside_it_from_shared_metadata` |
| A branch is not handed off ungated against the base it will merge into | a `Done` moves the run into `gating` with its claim held; `GitGate` rebases first and runs the commands on the rebased tree | `a_done_verdict_is_gated_in_its_own_worktree_before_the_claim_is_released`, `the_branch_is_rebased_onto_the_base_before_the_gate_runs_on_the_rebased_tree` |
| A rebase conflict is a human's problem, not a silent failure | the rebase is aborted and the issue parks `Blocked` naming the paths, in `last_error` | `a_rebase_conflict_parks_the_issue_blocked_naming_the_conflicted_paths`, `a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths` |
| The gate cannot become a runaway | a failing command is a `Continue` that carries its output, bounded by `gate.max_failures` consecutive failures → `Blocked`, and by `gate.timeout_ms` per gate | `a_failing_gate_continues_the_run_with_the_failing_output_in_hand`, `repeated_gate_failures_escalate_to_blocked_rather_than_looping`, `a_gate_that_hangs_is_killed_at_the_timeout_and_counts_as_a_failure` |

The three broker rows are one property in three places, and the middle one is the easy one to
lose: a reviewer who sees `max_calls_per_run` will read it as the bound and delete the
per-issue cap as redundant. It is not — re-read the continuation loop before touching it.

Three of these — the verdict, the per-issue turn budget and `parked_state` — are independent
brakes on the same runaway. Removing any one of them looks safe because the other two still
hold. They cover different paths; keep all three. The gate's `max_failures` is a fourth, for the
route the gate opened: a gate-sent `Continue` goes through the turn budget too, but at forty
turns a session the budget is a slow brake, and a suite an agent cannot make pass would spend
all of it rediscovering that.

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

Every seam symphony-cc needs to dispatch against its own backlog now has a real
implementation: `GitWorktreeWorkspace`, `TasksProjector`, `GithubTracker`
(`tracker.kind = "github"`), `ClaudeWorker` (`worker.kind = "claude"`), the tool broker
(`[broker]`, on by default), run transcripts (`[transcripts]`, likewise) and the handoff gate
(`[gate]`, on by default with `symphony.github.toml` naming the three commit-gate commands). The broker is on by default where the worker is not, because the
two switches mean opposite things: `worker.kind` decides whether an agent runs at all, while
the broker only decides whether an agent that is already running has a *scoped, logged* way to
do what it could otherwise do ambiently. Turning it off removes the audit trail, not the
authority. `symphony.github.toml` sets the first three; flipping `worker.kind` to `"claude"` in that same file is what turns
"list this repo's backlog" into "work it" — a decision left to whoever runs it, not a default.
`symphony.toml`, the default config, stays on `kind = "fake"` for both so the quickstart
experience is unchanged. When you change scheduler behaviour, ask whether the change would
still be correct when the agent running it is working on this repo.

[.mcp.json](.mcp.json) hands the agent rust-analyzer over MCP, so navigation in this repo is
LSP rather than grep — which is what makes the invariant table above checkable: whether
`guard_within` is still reached from both `prepare` and `remove` is a find-references
question. It needs three pieces on the host — the `rust-analyzer` server, `rust-src`, and the
`rust-analyzer-mcp` bridge — and how you install the first two depends on whether the
toolchain came from rustup or Homebrew, which is why `.claude/skills/setup-rust-analyzer`
exists rather than a command line here. A `SessionStart` hook
([.claude/hooks/rust-analyzer-check.sh](.claude/hooks/rust-analyzer-check.sh)) probes for all
three and names whichever is missing; without it the only symptom is an ENOENT at connect
time, which says nothing about which piece to install. Being committed, it is inherited by every
worktree under `.symphony/workspaces`, so each dispatched agent indexes its own copy of the
tree. That is intended, but it is not free — budget roughly 1-2 GB resident and one
`cargo check` per concurrent run when setting `agent.max_concurrent`.

One trap, and it bites exactly the use above: `references`, `definition` and `hover` answer
from whatever is indexed *so far* rather than waiting, so during the first load they come
back empty — and empty is indistinguishable from *no callers*, which reads as an invariant
that has already been broken. Only `rename` (which refuses outright) and `diagnostics`
(which carries a `complete` flag) wait for a quiescent workspace. Ask again until an answer
is non-empty before concluding anything from one.

## Constraints for the worker and broker

Decisions already taken that are expensive to rediscover. The first two are implemented in
[src/worker/claude.rs](src/worker/claude.rs), the third in [src/broker/](src/broker/):

- The worker's child environment is built from an explicit **allowlist**
  (`DEFAULT_ENV_ALLOWLIST`) — not inherit-and-scrub. A denylist is fragile, and one missed
  variable leaks a tracker credential into a coding agent. Notably absent by default: any
  tracker credential and any API key — an operator on API-key auth adds `ANTHROPIC_API_KEY`
  deliberately, it is not there by default.
- The worker execs the `claude` binary directly (`Command::new`, never a shell). **No `bash
  -lc`.** A login shell re-imports from the operator's dotfiles exactly the secrets that were
  just scrubbed.
- The MCP tool broker ([src/broker/](src/broker/)) executes tracker writes host-side while
  holding the credential. The worker receives results, never a raw token — and that is the
  exact extent of the claim. **The broker is not an isolation boundary, and the module doc says
  so.** Dropping `HOME` from the allowlist to close off `gh`'s stored token was tried and
  abandoned on evidence: on macOS `gh auth token` succeeds with `HOME` unset, because the token
  lives in the login keychain, keyed to the user session rather than to a path under `$HOME`.
  `claude`'s own OAuth credential behaves the same way — this dev machine has no
  `~/.claude/.credentials.json` at all and authenticates fine with `HOME` scrubbed. An
  environment allowlist cannot take away a credential that was never in the environment or the
  home directory; only a real sandbox (separate uid, container, seatbelt profile) could, and
  that is a different and much larger change. So a dispatched agent here *can* still comment,
  push and close as the operator through the ambient keychain. What the broker adds is a
  sanctioned path that is scoped to one issue, budgeted and logged — so the agent has no reason
  to reach for the ambient one, and every write it makes through the front door is auditable.
  Do not restate this as "the worker cannot reach a credential"; it is the weaker of the two
  options issue #4 offered, and it was chosen because the stronger one is not true on this
  platform.

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
