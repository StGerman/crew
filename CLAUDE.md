# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**MUST follow: [docs/coding-guidelines.md](docs/coding-guidelines.md).** Read it before the
first edit. It is the authoritative statement of how code is written here, with a check named
for every rule. This file covers what the system is and why; that file covers how to change it.

Development here is done by agents, so this file is the working surface: every command, every
switch, every trap. [README.md](README.md) is for a person deciding whether to adopt this and
what it is for — intention, installation, contribution. Commands belong here, not there.

## What this is

`crewd` is a tracker-driven orchestrator for Claude Code agents: a daemon that polls an
issue tracker, opens a workspace per issue, runs a coding-agent session in it, and reconciles
what comes back. It is a Rust reimplementation of the coordination layer described in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`, written after a review that
found several concrete defects in that design.

A lot of this code exists specifically in order *not* to have those defects. Read
**Invariants** before changing anything in `src/sched/`.

The workspace is split into packages (#45). `crewd`, at the root, is the daemon: its library is
`crew` (`use crew::`), its binary `crewd`. `crewctl/` is the client, a binary that links only
`libcrew/`, which holds what both need: the published `Snapshot`/`Row` types, the ops API's
address and marker, and the HTTP client and renderer. The standing rule is that anything both
binaries need moves *down* into `libcrew` and is never re-described in the client; the tell that
it has stopped being applied is a `pub` item in `libcrew` that `crewctl` never names. Because
`crewctl`'s dependency graph holds no `rusqlite`, `ratatui`, `tokio` or `ureq` — CI checks it
with `cargo tree` — "the client cannot open the daemon's store" is a fact about the build, not
an early `return` someone has to remember.

Slices 1–6 are complete and green: a deterministic core with a fake behind every external
seam, then real git worktrees and a real `~/.claude/tasks` projection, then a real GitHub
Issues tracker, then a real `claude -p` worker, then a host-side MCP tool broker that lets the
agent write to its own ticket without ever holding the credential, and an HTTP ops API over
the published snapshot with a `status` client in front of it for a person and an MCP server in
front of it for the agent supervising the daemon.

## Commands

```bash
cargo test                                 # every package; the count is in its output, never here
cargo test --lib                           # unit only
cargo test --test scheduler                # scheduler integration only
cargo test --test api                      # ops API integration only
cargo test a_permanent_failure             # one test; the arg is a substring match

cargo clippy --all-targets -- -D warnings  # the standing bar is zero warnings
cargo fmt --check

cargo run -- --tui                         # dashboard against the fake tracker
cargo run -- --max-ticks 20                # headless smoke run, then exit
cargo run -- --api 127.0.0.1:8787          # headless, with the ops API on for this run
cargo run -p crewctl -- status             # what a running daemon is doing, read over that API
cargo run -p crewctl -- status MT-649      # one issue in full: phase, attempt, turns, cost, branch
cargo run -- --mcp 127.0.0.1:8788          # the ops API's routes as MCP tools, for a supervising agent
claude mcp add --scope local --transport http crew_ops http://127.0.0.1:8788/ops
                                           # ...and how that agent gets them. Local scope, never user
cargo run -- init                          # register your own GitHub App: two clicks, writes ~/.crewd/
cargo run --example dashboard_preview      # render the UI to stdout, no terminal needed
cargo run --example broker_live            # real `claude` against a real broker; spends tokens
```

`cargo test`, `cargo clippy` and `cargo fmt --check` are the commit gate, and
[.github/workflows/ci.yml](.github/workflows/ci.yml) runs each as a required check on every
pull request rather than trusting whoever
remembers — delivery opens a pull request for every agent branch, so that is where a red gate
can still stop a merge. `default-members` covers every package, so each of those commands
checks the client and the library as well as the daemon, and `default-run` keeps a bare
`cargo run` meaning `crewd`.

The loop while editing, measured on this tree (an agent runs the command the docs name, so this
table *is* the loop):

| Editing | Command | Takes |
|---|---|---|
| anything | `cargo check` | ~0.2s, ~1.5s after an edit |
| shared types, the client | `cargo test -p libcrew` | ~0.2s |
| scheduler, store, broker | `cargo test -p crewd --lib -- --skip workspace::` | ~1s |
| `GitWorktreeWorkspace` | `cargo test -p crewd --lib workspace::` | ~3s |
| `crewctl` | `cargo test -p crewctl -p libcrew` | ~0.6s |
| before committing | `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check` | ~9s |

Most of the unit-test time is the `workspace::` tests shelling out to real `git`. The
package split bought enforcement, not build speed; `--skip workspace::` is what makes the common
case fast. `rust-toolchain.toml`
pins the compiler so CI, this machine and every worktree agree on what "it compiles" means, and
CI builds `--locked` so a drifted `Cargo.lock` fails rather than being quietly rewritten. One
thing to preserve if you edit that workflow: it must never *run* an example. `broker_live`
spawns a real `claude` and spends tokens, and only a Cargo default (examples are `test = false`)
keeps `cargo test` from calling it — the workflow says so at the top.

`CREW_DB=/tmp/x.db` points the store somewhere disposable — worth doing before any run
that might write state you do not want kept. `CREW_TASKS_ROOT=/tmp/tasks` does the same
for the `~/.claude/tasks` projection, so a smoke run's demo issues (`iss-001`, `MT-601`, ...)
don't land in your real Claude Code task list. `RUST_LOG=crew=debug` raises the log
level; logs always go to stderr, because under `--tui` the alternate screen owns stdout.

Every run's raw event stream lands under `<workspace.root>/.transcripts/`, one `.jsonl` per
run, and the `dispatched` log line names the file. That is the first thing to reach for when
asked what a run actually did — `jq -c 'select(.type=="assistant")' <file>` for the turns,
`grep crew_run_end` for how it exited — `symphony_run_end` in transcripts written before #45.
Tune or switch it off under `[transcripts]`; the bounds there are what make it safe to leave on.

Headless runs now create real `git worktree`s under `workspace.root` (default
`.crew/workspaces`, gitignored) against `workspace.repo` (default `.`) and real files
under `~/.claude/tasks/<derived-session-id>/`. Both are best-effort seams — a worktree or
projection failure degrades the run, it does not stop it — but they are real disk and git
state, not a simulation, so use the env overrides above when you just want to watch the
scheduler and don't want the side effects.

One thing the overrides do not cover: running the daemon from *inside* a worktree — which is
what a dispatched agent's cwd is. `GitWorktreeWorkspace::new` refuses that at startup, because
`workspace.repo = "."` there is a linked worktree and the worktrees a run would create register
in the top-level checkout's shared `.git`, where the orchestrator owning it never recorded them
(#29). The error names the way out: point `workspace.repo` at a throwaway clone and
`workspace.root` beside it. Setting `CREW_DB` alone does not help — the litter was never
in the store.

```bash
GITHUB_TOKEN=$(gh auth token) cargo run -- --config crew.github.toml --max-ticks 3
```

points the tracker at this repo's own real Issues instead of the fake demo data —
`crew.github.toml` is checked in and ready to use, no token in it. It sets
`tracker.dispatch_label = "agent"`, so every open issue carrying the `agent` label is
dispatchable and an assignee is neither needed nor sufficient (`DispatchRule` in
`src/tracker/github.rs`; with the key unset, any assignee is the signal, as before #64).

`tracker.github_app` replaces `GITHUB_TOKEN` with a GitHub App identity (#64): it names a file
holding `app_id`, `installation_id` and `private_key_path`, and every tracker write, forge call
and branch push is then authored by the App. The token is a *source*, not a `String`
(`src/credentials.rs`): an installation token expires hourly, so `GithubApp` mints on the
injected clock and re-mints `REFRESH_MARGIN_MS` before expiry. The push reaches it through a
`git credential-store` file `publish` creates in a private temp directory and deletes after one
push (`PushCredentialFile` in `src/workspace.rs`) — never a URL (lands in `.git/config`), an
`http.extraheader` (lands in argv) or an environment variable. That keeps the token out of what an agent
reads by accident, not out of reach of one that goes looking: it runs as the same user as the
key file, which is the limit **Constraints for the worker and broker** already records. `Config::load` loads the file
and the key once (`check_github_app`, not the per-tick `preflight`), so a half-configured App is
refused by name at startup. It is commented out in `crew.github.toml`
until `~/.crewd/github-app.toml` exists on the host; an uncommented key with no file there
stops the daemon from starting.

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
         advance_deliveries                          ← unconditional too
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

`advance_deliveries` is reconciliation too — it reads the outside world (CI, review) about runs
already over and may queue a retry the gate below then decides whether to dispatch — and only
touches issues nothing else owns. It comes *after* `harvest_gates` in the tick and, more to the
point, after it in the life of a `Done`: with a gate attached, a run's `Done` enters `gating`
and only the gate's pass reaches the `Done` arm of `apply_outcome` that queues delivery, so the
branch delivery pushes is the rebased, re-gated one. That order is the stack #43/#42 were built
as — gate, then delivery — and
`a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing` is
what fails if it is ever inverted.

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
   `tokio::sync::watch` channel, and the TUI, the HTTP API and `crewctl status` render that
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

**The store is a cache of judgment, not a system of record.** Losing `crew.db` degrades
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
existing branch that does carry commits rather than `-B`-resetting it. What the agent had *not* committed when
its run was stopped is snapshotted by `remove` to a new ref under `refs/crew/wip/<issue key>/`
— keyed on the issue id, not the renameable identifier, one ref per snapshot so a second stop
cannot orphan the first, and outside `refs/heads/` so the gate, delivery and the merged check
see only the agent's own commits — and the next run is told every such ref and its diffstat
rather than handed them applied (#22). Every removal path goes through
`remove` after `kill` has confirmed the stop; `shutdown()` removes nothing, and the worktree it
leaves is snapshotted whenever a later cleanup finally removes it. Cleanup is triggered by
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
operator's own `claude` config has until a dedicated API key changes that trade-off. The model is
not inherited that way: `worker.model` and `worker.effort` become `--model` and `--effort` on
every attempt, a resumed one included, and each run row records what it was given (#36). Both
unset passes neither flag, which is the old behaviour exactly; `crew.github.toml` pins them.
`--fallback-model` is deliberately never passed — it would make the recorded model possibly
wrong. `Outcome`
beyond done/failed — `Continue`, `Blocked` — has no structural signal from the CLI to key off,
so the worker's prompt asks the agent to end its final message with `CREW_OUTCOME:
continue: <reason>` or `CREW_OUTCOME: blocked: <reason>`; the module doc has the reasoning,
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

A rejected `rate_limit_event` is the one other thing `run_reader` parses off the stream, added
for #29's sibling defect (#37): three ordinary-looking dispatches interrupted by the same
account-wide limit each quarantined on their own, after three retries inside ninety seconds
against a five-hour reset nineteen minutes away — the exact "quarantine a ticket a bad token had
nothing to do with" mistake `TrackerError::class()`'s doc already warns against, on the worker
side of the process instead of the tracker side. `ClaudeWorker::rate_limit()` surfaces the
signal independently of `Outcome` — the CLI still reports its own verdict, ordinarily `Failed`,
since the process exits with no explicit marker — and `harvest_finished` checks it *before*
`apply_outcome` even sees the outcome: a run interrupted this way charges no attempt and no
quarantine streak, and releases the claim with `Store::release_for_rate_limit` rather than
`release`, which is what lets the issue resume at the attempt and session it was already on
rather than looking like a fresh start. What pauses is dispatch itself — `Scheduler::rate_limited`
checked once per tick, between `sweep_parked` and the two dispatch steps — until the CLI's own
`resetsAt`, published on `Snapshot::rate_limit_pause` so `status` reads "waiting on a five-hour
limit until 09:00Z" instead of showing an idle daemon with no explanation. A `resetsAt` the
scheduler cannot trust — missing, or already behind the clock — degrades to the ordinary
`Failed` path rather than risking a pause nothing ever lifts, the same failure mode a clock skew
would otherwise turn into a silent, permanent stop.

**Every run leaves a transcript** ([src/transcript.rs](src/transcript.rs)). The reader copies
each `stream-json` line to a per-run file *before* deciding whether the parser has a use for it
— so the `system` and tool-call lines it drops, and the lines it could not parse at all, are
still there afterwards — then appends how the process exited and what it said on stderr. The
path is on the run row (`Store::run`, `runs_for`) and in the dispatch log line and the TUI
detail pane, so "show me what run X did" needs no knowledge of the layout. Three
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
`Blocked` naming the paths — unless every path is in `gate.agent_resolvable` (`CLAUDE.md`,
`docs/**` and `src/store/schema.rs` in `crew.github.toml`), where two branches appended to the
same list and the verdict is a `Continue` carrying the conflict as its brief (#111); a **failing command** is the agent's, so the verdict is `Continue` and
the output rides into the next spawn as `Feedback::Gate` on `Worker::spawn` — the same
parameter delivery's CI and review feedback travel on, so there is one channel and one rendering
site for "why this attempt exists" — where the real worker puts it in the prompt; and `gate.max_failures` **consecutive** failures escalate to
`Blocked`, because a gate that can be failed forever is the continuation runaway wearing a new
name. A `Blocked` reason now also lands in the store's `last_error`, so the dashboard shows why
an issue is parked instead of only the log. Setting no gate is a decision, not a degrade — unlike
the broker or the projector, a scheduler without one hands a `Done` to a human exactly as the
agent left it, so `main.rs` attaches one whenever `gate.enabled` is true (the default, with an
empty command list, which makes the default a rebase and nothing more) and the scheduler tests
attach `FakeGate` explicitly. `crew.github.toml` sets the commit-gate commands from **Commands**
above; the fake worker never commits, so under `crew.toml` every gate finds nothing to hand
off.

The ops API ([src/api/mod.rs](src/api/mod.rs)) is the second observer of that same published
snapshot: `GET /api/v1/snapshot`, `GET /api/v1/issues/:identifier`, `POST /api/v1/refresh`,
`POST /api/v1/unquarantine/:identifier`, `POST /api/v1/unblock/:identifier`. `Api` holds a
`watch::Receiver` and a command sender and no `Store`, so rule 3 is enforced by the type rather
than by discipline — and the `POST`s can express nothing the dashboard's `r`, `u` and `b`
keys cannot. Off by default (`[api]
enabled`, or `--api <addr>` for one run) and loopback unless `api.allow_public` says otherwise,
because the `POST` routes control agent execution. Deliberately *not* validated in
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
agent onto its worktree. `POST /unblock` (#108) is the same shape for a park: it is how a
`Blocked` issue — a gate's rebase conflict, typically — is handed back once a human has resolved
it, since with `active_states = ["open"]` the tracker has no state to move it through. It clears
`parked_state` and the parked note and nothing else, so the next `dispatch_new` claims it the
ordinary way and `prepare` attaches to its branch; `Store::unblock`'s `WHERE` refuses anything
running, gating (a held claim is phase `running`), retry-queued, quarantined, or parked under a
delivery still `pending`, `awaiting` or `ready` — which would push or hand back the branch in the
tick an agent is dispatched onto it — or `handed_off`, whose branch is the operator's. `Scheduler::unblock`
also reads the ticket fresh and keeps a park whose ticket is no longer active, since
`sweep_parked` — which reclaims a closed ticket's worktree — only walks parked rows. Write what
changed into the issue's description first: that is the prompt the next run reads.

`crewctl status` ([crewctl/src/main.rs](crewctl/src/main.rs), over
[libcrew/src/client.rs](libcrew/src/client.rs) and [libcrew/src/render.rs](libcrew/src/render.rs))
is the other end, and it exists because the API on its own was not enough: it had been able to
answer for hours at the moment diagnosis instead went to a block-buffered log file and got the
wrong answer (#24). Nothing was missing server-side — what was missing was something to type.
So it is a client and nothing else, in its own binary: an operator asking what is running must
not be able to disturb it, and a second process on `crew.db` while the daemon holds it would be
exactly that. `crewctl` links no `Store`, worktree or tracker code at all — see the package
split under **What this is** — and its HTTP is a single `std::net` GET rather than an HTTP
crate, so its graph stays that small. It shares `Snapshot` and `Row` with the server through
`libcrew` instead of re-describing them, so a renamed field fails the build rather than
rendering a blank column, and it uses the same `fmt_count`/`fmt_ms`/`Phase::label` as the
dashboard so a duration means the same thing on all three surfaces.

Two behaviours there are load-bearing and easy to "simplify" away. It finds the daemon itself —
`--api`, then `[api] bind`, then `DEFAULT_API_BIND` — and reads the config *leniently* rather
than through `Config::load`, because that runs `preflight`, and preflight gates dispatch: an
unset `tracker.owner` is a real problem for the daemon and none at all for a client asking what
the daemon is doing. And `StatusError` keeps apart the two failures that a stack trace makes
look identical: nothing listening (no daemon — or one with its API off, which is the default
and so the likelier reading) versus a daemon that answered and refused. Collapsing those is
what has somebody restart a daemon that was never down, so each message names the address
tried, where that address came from, and the way out.

The ops MCP server ([src/api/mcp.rs](src/api/mcp.rs)) is the ops API's routes for an *agent*
supervising the daemon — the one driving a dogfooding session, a watchdog later — which the
`status` client left on the wrong side of the gap it closed: an agent had to spawn the CLI and
parse a rendering built to read well to a person. One tool per route (`snapshot`, `issue`,
`refresh`, `unquarantine`, `unblock`) and nothing else; each runs the *same* `Api` method the HTTP router
runs and frames the same `Response` as a tool result, a status of 400 or more becoming
`isError: true`. So it holds an `Api` and no `Store`, and it cannot express an authority the
HTTP API lacks — new authority is a separate decision from new transport, and this module has
nowhere to put one. `api.mcp_enabled` or `--mcp <addr>`, off by default, loopback unless
`api.allow_public`, through the same `resolve_bind` the HTTP API uses. The transport is the
broker's hand-rolled server made generic over `McpService` rather than copied or replaced with
`rmcp`; `src/broker/server.rs`'s module doc records what that cost.

That transport spends a thread per connection, and this is the first listener on it whose
address an operator chooses — `allow_public` can put it on a routable interface, where a client
that connects and then says nothing would hold a thread for free. `broker::server::Limits` is
the HTTP API's `READ_TIMEOUT` arriving here: a deadline on a request that has begun and never
ends, deliberately *split* from the idle wait between requests so keep-alive still works (the
real client depends on it), and a cap on connections in flight, because a client that
reconnects rather than dribbles pays nothing for a deadline. The broker's own listener gets
both for free and keeps a separate count, so a flood at the public address cannot starve a
dispatched run of its tools.

**This server must never reach a dispatched agent**, and that is the thing to hold when
touching any of this. The broker gives a worker authority scoped to one issue; this is scoped
to the whole daemon, and a worker that could call `unquarantine` or `unblock` could clear its own
quarantine or park and re-dispatch itself, defeating the verdict, `max_turns_per_issue` and `parked_state`
together. What enforces it is wiring, not a check on the tool: the ops server binds **its own
listener** (never the broker's — a shared one routed by prefix would put these tools at the
exact `host:port` every worker is handed), answers only at `/ops`, and is never passed to
`Broker`, whose `open` writes the only `--mcp-config` a worker receives.
`a_dispatched_worker_is_not_handed_the_ops_tools` reads that file from a real session and
connects to what it names. What wiring cannot enforce is the operator's own `claude` config:
the worker runs without `--strict-mcp-config` on purpose, so it inherits *user-scope* MCP
servers. Register this one in the supervising agent's **local or project scope, never user
scope**, or every worker inherits it and the wiring is bypassed by configuration.


**`crewd init`** ([src/init/](src/init/)) registers the operator's own GitHub App through the
App Manifest flow (#65): a loopback page posts the manifest to GitHub, the operator clicks
*Create*, GitHub redirects back with a single-use code, and `init` exchanges it for the App's
id and private key, then sends the browser on to *Install* and reads the installation id back
over the App's own JWT. It writes `~/.crewd/github-app.pem` (600) and `~/.crewd/github-app.toml`
(`app_id`, `installation_id`, `private_key_path`) in a 700 directory — the file #64's
`tracker.github_app` names — and never edits the daemon's config or overwrites either file.
Each operator registers their own App because the key is the App owner's; `GITHUB_TOKEN` and
a hand-registered App written into the same file stay supported. The listener reuses the broker
transport's `read_request`, `Limits` and `ConnSlot` cap, not its MCP service: it binds loopback, answers only
its own `Host`, and is joined shut once a callback carrying a code arrives. The GitHub calls go
over the tracker's `Http` seam, so the whole flow is tested against a fake GitHub over a real
socket; the real two-click run is the operator's. The conversion response is the one place in
the crate that carries a private key and a client secret, and the types are what keep it out of
the logs: the key sits in a `Pem` whose `Debug` redacts, and the secrets have no field at all.

**Delivery** ([src/sched/delivery.rs](src/sched/delivery.rs), behind the `Forge` and
`Publisher` traits in [src/forge/](src/forge/)) is what happens after a run reports `Done`,
when `[delivery] enabled` is on. `Done` still releases the claim and parks the issue exactly as
before; delivery is then a row in the store advanced on the tick — after reconciliation,
before the dispatch gate, at `delivery.poll_interval_ms` — through: push the branch
(`Publisher`, implemented by `GitWorktreeWorkspace`, from the worktree), open or find the pull
request (`Forge`, `GithubForge` over the tracker's `Http` seam), request the configured
reviewers *and read back whether they attached*, read CI, read the review threads. A red CI or
an open comment sends the issue back to an agent by the same path a `Continue` takes — a retry
due now, the session resumed, and the failure in the prompt as `Feedback::Ci` or
`Feedback::Review` — which is the literal form of "a red gate is a `Continue`, never a `Done`".
The handoff gate's failing output travels the same way, as `Feedback::Gate`: `launch` builds one
`Feedback` from delivery's structured row when there is one and from the retry reason otherwise,
so `Worker::spawn` has a single parameter for the question and `feedback_help` in the worker is
the single place its wording lives. And delivery only ever sees a branch the gate has passed —
see the tick order above. The pull request body is derived
from the run record (commits, runs, turns, tokens), never composed by the agent. Verdicts on
review comments come back as `CREW_REVIEW: <id>: accepted: <commit>` or `rejected:
<reason>` lines in the agent's final text, the same soft convention as `CREW_OUTCOME`; the
orchestrator replies on the thread and, once that reply has landed, records the verdict in
`review_verdict`, and a settled thread is never handed out again. A later step of the same
poll resolves that thread on the provider (#89) — GraphQL, since REST cannot — and a resolve
that fails is retried next poll without a second reply. A comment the agent gives no
line for stays open — and so does one whose acceptance names no commit, or a commit the branch
does not carry: the worker drops an `accepted:` whose detail is not shaped like a commit, and
the scheduler checks the rest against the branch (`Publisher::carries`) at the moment the run
reports `Done`, before the gate's rebase rewrites the shas the agent named.

Four things there are load-bearing. Every hand-back is a *round*, bounded per pull request
(`max_rounds_per_pr`) and per issue (`max_rounds_per_issue`), and the per-issue count never
resets — not for a new run and not for a new pull request — because a reviewer that comments
on every push, answered by an agent that pushes, is a loop with no bound of its own, and one
that reset with the pull request would bound nothing (the same gap `max_calls_per_issue`
closes for the broker). At either bound the pull request is handed to the operator with the
outstanding items named. A review request is followed by a read of the pull request and its
reviews, because GitHub answers a request for a bot reviewer with `200` and attaches nobody
(GETT-174120); a request that verifiably attached nobody is a handoff with that reason on the
issue's row, not a success. `Forge` has no `merge` method, and must not grow one — merging is
the operator's, and the trait's shape is what enforces it. And a delivery step only runs for an
issue nothing else owns (phase `released`, no live run), so a push cannot land under a running
agent and a hand-back cannot race a dispatch. A branch whose work sits on another issue's branch
gets its pull request based on that branch (`Publisher::stacked_on`), so the two stay
reviewable apart — provided the remote has that branch. A lower branch still running, or done
and not yet pushed, is not a base a pull request can be opened against, so the upper one opens
against the trunk rather than handing off on the provider's 422; and when a later push computes
a different base than the open pull request targets — the lower branch merged — `open_pull_request`
retargets it, body included, so the store, the snapshot and the provider agree.

The review on PR #42 found the handoff itself wrong in six places (#47), and two of them are
worth knowing the shape of before touching `publish` or `apply_verdicts`. The push is
`--force-with-lease`, because the gate rebases an already-published branch before every
re-delivery and a plain push then fails non-fast-forward in exactly the round the base moved;
forcing is sanctioned because the branch is the orchestrator's own, and the lease is what keeps
that apart from forcing over someone else's — a lease failure is classified on its own,
permanent, naming the remote branch that moved. And a verdict is settled by its reply landing
and by nothing else: `apply_verdicts` replies first and records second, a failed reply leaves
the verdict queued on the row and the step returns the error, so the threads are not read — or
handed back to an agent — until it lands. Recording first, as it did, hid a verdict from its
reviewer for good over one transient error while delivery went on to `Ready`.

## Invariants

Each of these closes a defect found in the original spec. The later rows came instead from
dogfooding this orchestrator against its own backlog: the first review, the operator surface
built over the same state, issue #1's acceptance criterion made executable, the first live
dispatch and the review that followed it, the client put in front of that operator surface,
making a finished run diagnosable, a closed ticket whose worktree outlived it, an
orchestrator that ran inside its own worktree, the agent supervising the daemon getting
the same surface as data, two branches that were each green alone and broken together, and
the review of the handoff path that found it handing off, retrying or settling in six cases
where it should not.

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
| A label's casing cannot make an issue undispatchable | `GithubTracker::to_issue` trims, lowercases, drops blank and dedupes labels — the shape `Config::normalize` gives `required_labels` — so `routable`'s plain equality holds; `set_state` reads the raw labels instead, so writing them back never renames the operator's own (#70) | `labels_are_normalized_at_the_adapter_boundary`, `set_state_writes_labels_back_in_their_original_casing` |
| An installation token cannot expire under a long-running daemon | the tracker, forge and push ask a `Credentials` source per request; `GithubApp` caches against the injected clock and re-mints `REFRESH_MARGIN_MS` before expiry; a 401 on an App token — or a push git reports refused for authentication — re-mints and repeats the refused request exactly once, so an early revocation neither fails a poll nor hands delivery off (#64) | `a_daemon_up_past_the_installation_tokens_lifetime_keeps_polling_without_a_401`, `an_installation_token_is_re_minted_before_it_expires_rather_than_served_stale`, `a_token_revoked_before_its_expiry_is_replaced_and_the_pull_request_still_opens`, `a_credential_refused_twice_is_permanent_after_exactly_one_retry`, `a_push_refused_for_authentication_is_retried_once_on_a_fresh_token`, `an_authentication_refusal_that_survived_its_retry_is_permanent` |
| The push credential never reaches the agent sharing its worktree | `publish` hands git a `credential-store` file in a private temp dir, deleted after the push, behind an empty `credential.helper` that clears the operator's own — never a URL, `extraheader` or env var | `a_push_credential_reaches_git_without_touching_config_argv_or_a_lasting_file`, `the_default_allowlist_names_no_credential_variable` |
| A teammate's assignment does not hand an issue to an agent | with `dispatch_label` set, `DispatchRule` makes the label the whole signal; unset, any assignee, as before | `with_a_dispatch_label_only_the_label_makes_an_issue_dispatchable`, `without_a_dispatch_label_any_assignee_is_still_the_signal` |
| A half-configured App is refused by name, not met as a 401 | `Config::load` runs `check_github_app` once — never per tick — loading `tracker.github_app` and its key and naming each missing field or unreadable file | `a_half_configured_github_app_is_refused_naming_the_missing_piece` |
| A workspace path cannot escape its root | `guard()` on **both** `prepare` and `remove` | `hostile_identifiers_stay_inside_the_root` |
| Cleanup cannot discard an agent's commits | `branch -d` (not `-D`) on remove; attach, not `-B`, on reuse | `a_branch_holding_committed_work_outlives_the_worktree_it_is_removed_with` |
| Cleanup cannot discard an agent's *uncommitted* work | `remove` snapshots a dirty tree to a new ref under `refs/crew/wip/<issue key>/` (never the branch, never overwriting an earlier snapshot) before deleting it, failing closed; `prepare` reports every such ref and the next prompt names them | `a_worktree_removed_with_uncommitted_changes_leaves_them_recoverable_from_its_wip_ref`, `a_run_killed_with_uncommitted_changes_has_them_recoverable_after_its_workspace_is_removed` |
| A dead session cannot strand an issue | drop the session name after a run with zero turns | `a_run_that_took_no_turns_is_not_retried_into_the_same_conversation` |
| A hard kill cannot strand a claim | startup `recover()`: a claim with no live run is stale, because `running` cannot cross a process boundary | `a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup` |
| A hard kill cannot zero an in-flight run's progress | `observe_progress` checkpoints `run.turns` once per tick when the count moved, and `close_open_runs` keeps it and charges it to `cumulative_turns` in one transaction | `a_run_interrupted_by_a_hard_kill_reports_its_last_known_turn_count_after_restart` |
| An agent cannot write to another ticket | no tool takes an issue id; the target comes from the per-run token | `a_call_naming_a_different_issue_is_refused_and_the_refusal_is_audited` |
| A looping agent cannot write without bound | per-run **and** per-issue budgets, charged on attempts not successes | `a_continuation_cannot_refresh_the_budget_by_opening_a_new_session` |
| A finished run keeps no write authority | the session is an RAII guard living in the `running` entry | `a_run_that_ends_takes_its_broker_authority_with_it` |
| Clearing a quarantine or a park cannot release a live claim | `Store::unquarantine` is guarded on `quarantined_at IS NOT NULL`, and `Store::unblock` on a park in phase `released` with no quarantine, no retry row and no delivery in a stage that pushes, hands back or has handed off — so a running, gating, retry-queued or delivering issue is untouched, and neither is one whose ticket `Scheduler::unblock` reads as no longer active — and both report what they did; unblock lifts the park and never takes or releases a claim (#108) | `clearing_a_quarantine_that_is_not_there_does_not_release_a_live_claim`, `unblocking_an_issue_that_is_not_parked_does_not_release_a_live_claim`, `unblocking_a_parked_blocked_issue_dispatches_it_onto_its_existing_branch`, `an_unblock_does_not_lift_a_park_a_live_delivery_still_owns`, `unblocking_an_issue_whose_ticket_closed_leaves_it_for_the_parked_sweep`, `unblocking_an_issue_dispatch_would_refuse_keeps_its_park`, `a_ticket_that_closes_between_an_unblock_and_the_next_tick_is_still_swept` |
| A slow HTTP client cannot delay a tick | one task per connection, a `oneshot` reply the scheduler never waits on, and a bounded read timeout | `a_client_that_never_finishes_its_request_cannot_delay_a_tick` |
| The projection cannot become load-bearing | `publish` logs a projector error and returns `Ok`; nothing written is ever read back | `the_scheduler_makes_the_same_decisions_whether_the_projector_writes_fails_or_is_off` |
| A killed run cannot record a fabricated cost | totals are read only from the `result` event; a run that never emits one stores NULL, not a per-event sum | `a_run_that_dies_before_its_result_event_reports_no_token_total` |
| A half-applied migration cannot stop the store opening | each migration and the `user_version` bump that records it commit in one transaction | `a_migration_that_fails_partway_leaves_no_trace_and_does_not_advance_the_version` |
| A released migration is never edited | `migrate` records only the number of the last migration a store applied, so the tests pin a `blake3` of every released entry in `RELEASED`: an edited entry fails naming its version, and a new one fails until its hash is appended (#81) | `a_released_migration_is_never_edited` |
| "No daemon" is never confused with "daemon said no" | `StatusError` splits a refused connection from a refused request, and names the address and its source in both | `a_closed_port_reads_as_no_daemon_rather_than_a_refused_request` |
| The client cannot open the daemon's store | `crewctl` is its own package over `libcrew`, whose graph holds no `rusqlite`, `ratatui`, `tokio` or `ureq`; CI fails if `cargo tree -p crewctl -e normal` ever shows one (#45) | `a_status_query_does_not_open_the_database_the_daemon_holds` |
| The branch an operator is sent to is the one git checked out | `Workspace::branch_for` is the same naming function `prepare` uses, not a second spelling of it | `the_branch_the_snapshot_publishes_is_the_one_prepare_checks_out` |
| The published branch never names a ref that is gone or was never this run's | `Store::set_branch` persists what `prepare` returned and is cleared exactly when `Removed::branch_deleted` says cleanup deleted it — never recomputed from `identifier`, which `Store::ensure` can rename after dispatch | `the_published_branch_is_the_one_prepare_recorded_not_one_recomputed_from_the_current_identifier` |
| Retention cannot delete a live run's transcript | `prune` is handed the paths of runs still in `running` | `retention_bounds_the_transcript_directory_but_spares_a_stalled_runs_own_file` |
| The ops tools never reach a dispatched worker | the ops MCP server has its own listener and its own path, and is never passed to `Broker`, whose `open` writes the only `--mcp-config` a worker is handed | `a_dispatched_worker_is_not_handed_the_ops_tools` |
| An operator's question cannot disturb the daemon it asks about | `OpsMcp` holds an `Api` — a `watch::Receiver` and a `Command` sender, no `Store` — and a read sends no `Command` at all | `an_ops_read_cannot_disturb_the_daemon_it_asks_about` |
| A wedged or hostile client cannot exhaust the MCP transport | `broker::server::Limits`: a deadline on a request that has started, split from the idle wait so keep-alive survives, plus a per-listener cap on connections in flight, released by RAII | `a_client_that_never_finishes_its_request_cannot_hold_a_connection_thread`, `a_flood_of_connections_is_refused_rather_than_served_without_bound` |
| A parked run's worktree is reclaimed once its ticket closes | `sweep_parked` re-reads parked ids on a bounded cadence, unparks what it cleans, and clears the published branch when cleanup deleted the ref | `a_parked_issue_that_is_later_closed_has_its_workspace_reclaimed_without_a_restart`, `sweeping_parked_issues_costs_tracker_traffic_bounded_by_the_interval_not_by_ticks`, `a_sweep_that_deletes_a_branch_clears_the_name_the_snapshot_publishes` |
| One orchestrator cannot nest its worktrees inside another's | `GitWorktreeWorkspace::new` refuses a `repo` or `root` inside a linked worktree of the repository; `remove` prunes registrations beneath the path it deletes, then `branch -d`s their branches with the merged check intact | `an_orchestrator_cannot_be_started_inside_another_runs_worktree`, `removing_a_worktree_reclaims_the_worktrees_nested_inside_it_from_shared_metadata` |
| A branch is not handed off ungated against the base it will merge into | a `Done` moves the run into `gating` with its claim held; `GitGate` rebases first and runs the commands on the rebased tree | `a_done_verdict_is_gated_in_its_own_worktree_before_the_claim_is_released`, `the_branch_is_rebased_onto_the_base_before_the_gate_runs_on_the_rebased_tree` |
| A rebase conflict is a human's problem, not a silent failure — unless every conflicted path is one the operator listed as the agent's | the rebase is aborted first, whichever way it goes, and one left in progress after the abort is `Verdict::Stuck`, always `Blocked`; a conflict touching any path outside `gate.agent_resolvable` parks `Blocked` naming the paths, in `last_error`; one confined to it is a `Continue` whose `Feedback::Gate` brief names the base and paths, charged to `gate.max_failures` like a failing command (#111); `preflight` rejects a blank pattern | `a_rebase_conflict_parks_the_issue_blocked_naming_the_conflicted_paths`, `a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths`, `a_conflict_confined_to_agent_resolvable_paths_is_handed_back_to_the_agent`, `a_conflict_touching_any_other_path_still_blocks_for_a_human`, `repeated_unresolved_docs_conflicts_escalate_to_blocked`, `a_blank_agent_resolvable_pattern_is_rejected`, `a_rebase_the_gate_could_not_abort_blocks_for_a_human_even_on_resolvable_paths`, `a_worktree_left_mid_rebase_is_detected_and_an_aborted_one_is_not` |
| The gate cannot become a runaway | a failing command is a `Continue` that carries its output, bounded by `gate.max_failures` consecutive failures → `Blocked`, and by `gate.timeout_ms` per gate | `a_failing_gate_continues_the_run_with_the_failing_output_in_hand`, `repeated_gate_failures_escalate_to_blocked_rather_than_looping`, `a_gate_that_hangs_is_killed_at_the_timeout_and_counts_as_a_failure` |
| A restart cannot forgive the gate's failure streak | the streak is `issue_state.gate_failures` (v7), bumped by `Store::bump_gate_failures` and cleared exactly where a pass or a verdict ending the line of work cleared the old in-memory map | `a_gate_failure_streak_is_not_forgiven_by_restarting_the_daemon` |
| A gate cannot outrun the concurrency limit | `global_slots` and `state_slots` count `gating` as well as `running`, because a gate is a build on this host and the claim is held across it | `a_gating_run_still_holds_its_concurrency_slot_so_gates_cannot_accumulate` |
| A continuation keeps its place in the milestone order | a retry queued by a `Continue` verdict — the agent's or the gate's — records the state whose slot it holds (`retry.reserved_state`, v10), and `global_slots`/`state_slots` count it for the length of its delay; a failure backoff, a delivery hand-back and a rate-limit release reserve nothing; the reservation lives on the retry row, so every path deleting the row ends it, and `dispatch_due_retries` re-reads a waiting reservation each tick so a ticket closed or unlabelled during the delay frees its slot at once — while an id the tracker merely omits keeps its reservation until due, one omission being the blip `refresh_miss_grace` exists for; a due reservation counts only the reservations ahead of it in due order, so ones oversubscribing a limit after a state change dispatch earliest-first instead of deferring each other forever; the snapshot publishes `reserved` and `Row::holds_slot` (#86) | `a_continuing_issue_keeps_its_slot_through_its_continuation_delay`, `a_continuation_holds_its_slot_in_its_own_state_limit`, `a_failure_backoff_does_not_hold_a_slot`, `a_continuation_whose_ticket_went_terminal_releases_its_reserved_slot`, `a_waiting_continuation_survives_one_tracker_omission_with_its_slot`, `reservations_oversubscribing_a_state_limit_dispatch_in_due_order_rather_than_deadlocking` |
| A gate cannot hide how far its run got | the turn count is checkpointed as the entry leaves `running`, since `observe_progress` walks only that map while the run row stays open for the whole gate | `a_run_entering_the_gate_checkpoints_its_turn_count_before_it_leaves_running` |
| A gate command that can never start is refused at startup | `preflight` rejects a blank program name as well as an empty argv, so a typo costs one error instead of every run of the issue | `a_gate_that_could_never_escalate_or_never_start_is_rejected` |
| The gate's own git subprocesses are killable | every `git` the gate runs goes through `spawn_tracked`, so `pgid` is set for the rebase and not only for a configured command | `killing_a_gate_during_the_rebase_step_stops_the_git_subprocess_instead_of_leaving_it_running` |
| A gate that cannot start reports rather than panics | a supervising thread the OS refuses becomes `Verdict::Failed`, which is what `Gate::start` promises; a panic here would strand the claim until the next startup | `a_gate_whose_supervising_thread_cannot_be_spawned_reports_failed_instead_of_panicking` |
| A brief describes the tree the agent will find | `Verdict::Failed` carries `on_base`, false for every step before the rebase and for a rebase that was aborted | `a_gate_that_failed_before_rebasing_does_not_tell_the_agent_its_branch_was_rebased` |
| A red gate cannot rest on `Done` | delivery reads CI after every push and hands a failure back through the retry path with the failure in the prompt | `a_red_ci_gate_re_dispatches_the_issue_with_the_failure_in_the_prompt_and_the_run_does_not_rest_on_done` |
| A review request that attached nobody is not a success | the request is followed by a read of `requested_reviewers` and `reviews`; a missing login hands off with the reason | `a_review_request_the_provider_accepts_without_attaching_a_reviewer_is_reported_as_a_failure` |
| A settled review comment is not re-argued | verdicts are recorded by comment id, first one stands, and open threads are computed against that table | `each_review_comment_ends_accepted_with_a_commit_or_rejected_with_a_reason_and_is_not_re_argued` |
| The review-fix loop is bounded, and the bound survives a new run and a new pull request | `rounds_pr` resets only when the pull request number changes; `rounds_issue` never resets; both checked before charging | `fix_rounds_are_bounded_per_pull_request_and_per_issue_and_the_bound_survives_a_new_run_and_a_new_pull_request` |
| Nothing merges without a human | `Forge` has no merge method; a ready pull request is polled, never advanced | `nothing_merges_without_a_human` |
| Delivery never publishes an ungated branch | a `Done` enters `gating` first and only the gate's pass reaches the `Done` arm that queues delivery; a failing gate is a `Continue` the forge never hears about | `a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing` |
| A reused pull request targets the base the scheduler recorded | `open_pull_request` compares the base it finds with `spec.base` and retargets — body included — when they differ, instead of returning the pull request as found | `a_reused_pull_request_whose_desired_base_has_changed_is_retargeted`, `a_pull_request_whose_desired_base_has_changed_is_retargeted_and_the_snapshot_agrees` |
| A stack base the remote does not have is not a base | `Publisher::stacked_on` takes the remote and keeps only the candidates it has, asked directly with `ls-remote` rather than read off remote-tracking refs | `a_stack_candidate_the_remote_does_not_have_is_not_selected_as_a_base`, `a_base_that_is_not_published_is_not_selected_as_a_stack_base` |
| A verdict is settled only by a reply that landed | `apply_verdicts` replies first and records second; a failed reply leaves the verdict in `pending_verdicts` — which the push no longer clears — and returns the error, so the threads are neither read nor handed back until it lands | `a_verdict_is_not_settled_by_a_reply_that_did_not_land` |
| A settled review comment's thread is resolved, and resolution never re-opens the verdict | `resolve_settled_threads` runs after `apply_verdicts` as its own step, resolving every recorded verdict — accepted or rejected — whose `review_verdict.resolved_at` (v9) is NULL; a failed resolve is logged and retried next poll, costing no reply, no round and no readiness — but surfaced, a permanent refusal on the issue's row — which stays computed from the verdict table rather than the provider's `isResolved`; `GithubForge::resolve_threads` reads the pull request's threads over GraphQL once per pass, finds each by its root comment's `databaseId`, is a no-op on one already resolved, and counts a mutation answered without `isResolved: true` as a permanent refusal rather than a resolve (#89, #100) | `a_settled_review_comment_has_its_thread_resolved`, `a_failed_resolve_is_retried_and_never_re_replies`, `a_refused_resolve_is_reported_on_the_row_without_handing_off_a_ready_pull_request`, `resolve_threads_finds_the_thread_by_its_root_comment_and_resolves_it`, `a_resolve_the_provider_did_not_apply_is_not_recorded_as_resolved`, `resolving_several_comments_reads_the_threads_once` |
| A handed-off delivery does not outlive its merged pull request as a failure | `advance_deliveries` keeps polling a `HandedOff` row at `poll_interval_ms`, for the pull request's state alone (`watch_handoff`: no push, no review request, no `Feedback`); merged or closed moves it to `Closed` with how it ended and clears the issue's `last_error`, through the same `close_delivery` a `Ready` row takes (#101) | `a_handed_off_pull_request_the_operator_merged_is_reported_closed_not_failed`, `a_handed_off_pull_request_that_stays_open_is_polled_for_its_state_only` |
| A new head is not left unreviewed | `set_delivery_pr` resets `review_requested` when the head changes, not only when the pull request number does, so the request-then-verify runs again for every push | `a_new_head_on_the_same_pull_request_needs_its_review_requested_again`, `a_fix_round_re_requests_review_so_the_new_head_is_not_left_unreviewed` |
| An acceptance names a commit the branch carries, never a bare acknowledgement | `extract_verdicts` drops an `accepted:` whose detail is not commit-shaped; `verified_verdicts` checks the rest with `Publisher::carries` as the run reports `Done`, before the gate's rebase rewrites the shas | `an_acceptance_that_names_no_commit_leaves_its_comment_outstanding`, `an_acceptance_is_believed_only_for_a_commit_the_delivered_branch_carries`, `an_acceptance_naming_a_commit_the_branch_does_not_carry_leaves_the_comment_outstanding` |
| A rebased branch updates its pull request, and never overwrites someone else's work | `publish` pushes `--force-with-lease`; a lease failure is classified on its own as permanent, naming the remote branch that moved, so it stays distinct from a stale-base rejection | `a_rebased_branch_is_pushed_over_its_own_history_but_never_over_someone_elses` |
| An account-wide rate limit is not any one issue's failure | a rejected `rate_limit_event` releases the claim (`Store::release_for_rate_limit`, not `release`) without charging an attempt or the identical-failure streak, and pauses dispatch itself until `resets_at` rather than scheduling a per-issue retry | `a_rate_limit_pauses_dispatch_rather_than_quarantining_the_issues_it_interrupted` |
| A rate limit cannot stop dispatch on a clock the host disagrees with | a `resets_at` that is missing or already behind the clock falls through to the ordinary `Failed` path instead of pausing on a value that would never lift | `a_rate_limit_with_no_usable_resets_at_degrades_to_ordinary_backoff` |
| A misspelled `tracker.kind` or `worker.kind` cannot silently run the fake | both parse into `TrackerKind`/`WorkerKind` in `preflight`, naming the value and the supported set, and `main.rs` matches on the enum with no `else` fallthrough; an empty `worker.kind` is `fake` on purpose | `a_misspelled_tracker_kind_is_rejected_rather_than_running_the_demo`, `a_misspelled_worker_kind_is_rejected_rather_than_running_the_fake` |
| A run names the model that did its work, not today's setting | the scheduler records `Worker::model()` — the value the worker builds `--model`/`--effort` from — on the run row at `start_run`, and never updates it | `a_run_records_the_model_it_was_dispatched_with_rather_than_the_current_default` |
| A model the CLI refuses is not silently replaced by the default | `model_not_found` on the stream is `ErrorClass::ModelNotFound`, permanent; an unknown `effort`, which the CLI would ignore with a warning, fails config load | `a_model_the_cli_refuses_quarantines_the_issue_instead_of_retrying_it`, `a_model_setting_the_cli_would_silently_ignore_is_refused_at_load` |
| `crewd init` cannot be driven by a page other than its own | a per-run 128-bit `state` nonce checked on the callback — a mismatch ends the run before the code is converted — and a listener that answers only its own loopback `Host` | `a_callback_whose_state_this_run_did_not_issue_is_refused_without_converting_the_code`, `a_request_under_another_host_name_is_not_shown_the_page_or_its_nonce` |
| The init listener does not outlive its one callback | `await_callback` stops and joins the accept thread before returning, whatever the callback's outcome | `init_ends_with_a_600_key_a_settings_file_naming_it_and_an_installed_app_after_two_clicks` |
| The App's key and client secret never reach a log | the key is held in a redacting `Pem`, the secrets are never deserialized, and no error quotes a successful conversion body | `neither_the_key_nor_the_client_secret_reaches_the_log_at_any_level` |
| `init` never overwrites a key | both files checked before GitHub is asked anything, then created with `create_new` at mode 600 | `an_existing_key_or_settings_file_is_refused_by_name_before_github_is_asked_anything` |
| The created App has exactly the permissions asked for, and is not offered for install otherwise | one `PERMISSIONS` table builds the manifest and is compared against `GET /app`, read with the App's own JWT, while the callback is still open — only a pass redirects the browser to *Install* | `an_app_created_with_other_permissions_than_the_manifest_fails_the_run_over_its_own_jwt` |
| The init listener cannot be made to hold threads without bound | `ConnSlot::take` against `Limits::max_connections` before a connection's thread exists; past the cap the socket is closed | `connections_past_the_cap_are_refused_rather_than_each_given_a_thread` |

The delivery rows' bound is the same shape as the broker's, and each guard was checked the same
way: disable the mechanism — treat a CI failure as success, trust the provider's `200`, drop
the per-issue bound or reset it with the pull request, stop consulting the verdict table — and
the named test fails. The six rows from #47 were checked the same way — a plain push, then a
bare `--force`; the pull request returned as found; the remote filter dropped from both
`stacked_on`s; the verdict recorded before its reply; `review_requested` keyed on the number
alone; the commit check removed from the worker, from `carries` and from the scheduler in turn —
and each named test failed, with the file restored by `cp` rather than `mv`, because a preserved
mtime leaves cargo running the stale binary.

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

See [docs/coding-guidelines.md](docs/coding-guidelines.md), linked at the top of this file.

## Dogfooding

The backlog is GitHub Issues on this repository, labelled `agent` — which is exactly the shape
`TrackerConfig.dispatch_label` marks as ready. Work is picked up from there, not from a plan file.

Every seam crewd needs to dispatch against its own backlog now has a real
implementation: `GitWorktreeWorkspace`, `TasksProjector`, `GithubTracker`
(`tracker.kind = "github"`), `ClaudeWorker` (`worker.kind = "claude"`), the tool broker
(`[broker]`, on by default), run transcripts (`[transcripts]`, likewise) and the handoff gate
(`[gate]`, on by default with `crew.github.toml` naming the commit-gate commands) and
delivery (`[delivery]`, off by default and on in `crew.github.toml`, for the same reason
`worker.kind` is: it publishes under the operator's credentials). The broker is on by default where the worker is not, because the
two switches mean opposite things: `worker.kind` decides whether an agent runs at all, while
the broker only decides whether an agent that is already running has a *scoped, logged* way to
do what it could otherwise do ambiently. Turning it off removes the audit trail, not the
authority. `crew.github.toml` sets the first three; flipping `worker.kind` to `"claude"` in that same file is what turns
"list this repo's backlog" into "work it" — a decision left to whoever runs it, not a default.
`crew.toml`, the default config, stays on `kind = "fake"` for both so the quickstart
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
worktree under `.crew/workspaces`, so each dispatched agent indexes its own copy of the
tree. That is intended, but it is not free — budget roughly 1-2 GB resident and one
`cargo check` per concurrent run when setting `agent.max_concurrent`.

One trap, and it bites exactly the use above: `references`, `definition` and `hover` answer
from whatever is indexed *so far* rather than waiting, so during the first load they come
back empty — and empty is indistinguishable from *no callers*, which reads as an invariant
that has already been broken. Only `rename` (which refuses outright) and `diagnostics`
(which carries a `complete` flag) wait for a quiescent workspace. Ask again until an answer
is non-empty before concluding anything from one.

## Skills

Reach for each at the moment named:

- **`engineering:architecture`**: a decision has to be recorded before code (an issue's "Open"
  section, a new seam or trait, reversing a line in this file). The ADR goes in `docs/adr/`
  (#63), and the issue and the code cite it in one line.
- **`mattpocock-skills:diagnosing-bugs`**: a run, a scheduling decision or a test does something
  nobody can explain. Start from the transcript, `crewctl status <issue>` and the daemon log,
  and finish with a guard test that fails without the fix.
- **`/code-review`**: every pull request before it merges, as well as Copilot's review. It
  catches a second implementation of something that already exists, as on #88.

Watching a running daemon is `/supervise-crewd`, typed by the operator's session. It is never
loaded into a dispatched agent.

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
