# Coding guidelines

For: anyone, human or agent, who edits `src/` or `tests/` and wants the PR to pass review on
the first round.

This file is authoritative for how code in this repository is written. [CLAUDE.md](../CLAUDE.md)
covers what the system is, how to run it, and which invariants it protects. Read that file for
architecture. Read this one before the first edit.

Each rule has three parts: the rule itself, one sentence of why, and a place in the tree that
shows the rule done right. **MUST** is a merge blocker. **SHOULD** needs a reason in the PR when
it is skipped.

Rules describe the target state. A rule that the code does not yet meet carries a tag:
`Not yet enforced: #NN`. The linked issue tracks the fix, and new code follows the rule from
now. Remove the tag when the issue closes.

## Error handling and panics

- **MUST** use `anyhow` only at the binary boundary and `thiserror` enums inside library
  modules. Why: a typed error lets the scheduler classify a failure; an `anyhow` error can only
  be logged. Example: `ErrorClass::retryable()` in [src/model.rs](../src/model.rs) decides
  between retry and quarantine, and a new error kind must be classifiable there.
- **MUST NOT** call `unwrap()` or `expect()` on a production path, with one exception. An
  `expect("...")` whose message names a local invariant that the surrounding code just
  established is allowed. Why: a panic in the scheduler ends every run in flight. Example:
  `.expect("piped at spawn")` in [src/worker/claude.rs](../src/worker/claude.rs).
- **MUST NOT** call `lock().unwrap()`. Why: a panic while the lock is held poisons a
  `std::sync::Mutex`, and every later call on that lock panics for the rest of the process.
  Use `parking_lot`, whose locks do not poison. `Not yet enforced: #49`.
- **MUST** handle an error once. Either log it and continue, or propagate it with `?`. Never
  both. Why: a double-handled error appears twice in the log and once in the caller, and the
  reader cannot tell how many failures happened. Example: `log_tracker_failure` in
  [src/sched/mod.rs](../src/sched/mod.rs) logs and skips the tick; nothing above it logs again.
- **MAY** write `let _ = ...` on a best-effort seam, only with a comment that names why the
  result does not matter. Why: the projector, the transcript and the snapshot channel are
  designed to degrade rather than fail, and the comment is what distinguishes a decision from
  an oversight. Example: the `snap_tx.send` sites in [src/main.rs](../src/main.rs).
- **MUST NOT** use `panic!`, `unreachable!` or `todo!` in production code, except an
  `unreachable!` guarded by a validation step in the same function. Why: the guard is what
  makes the branch unreachable, and the message must name that guard. Example:
  `unreachable!("validate rejects unknown tools")` in [src/broker/mod.rs](../src/broker/mod.rs).

## Testing

- **MUST** name a test as a sentence that asserts the invariant, not `test_foo`. Why: if the
  name cannot say what the test defends, the test probably defends nothing. Example:
  `a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup` in
  [tests/scheduler.rs](../tests/scheduler.rs).
- **MUST** add a trait and a fake for every new external effect, in the same commit as the
  effect. Why: the scheduler tests drive real `Scheduler::tick()` calls with no network, no
  disk and no clock, and a seam without a fake is a path those tests cannot reach. Example:
  `Clock` and `FakeClock` in [src/clock.rs](../src/clock.rs).
- **MUST** move time through `FakeClock` in scheduler and API tests. `std::thread::sleep` and
  `Instant::now` are allowed only in the tests that drive a real child process or a real
  socket, in [src/worker/claude.rs](../src/worker/claude.rs) and
  [src/broker/server.rs](../src/broker/server.rs). Why: a test that waits on the wall clock is
  a test that flakes on a slow runner.
- **MUST** pair every row in the CLAUDE.md invariant table with a named guard test. A change
  that weakens a mechanism must first make its guard test fail. Why: several guard tests fail
  only in the exact scenario they were written for, so a green run is not proof that the test
  still means anything.
- **SHOULD** assert rendered text with an `insta` snapshot, not with `contains()`. Why: a
  substring assertion couples the test to wording, and a snapshot makes a wording change a
  reviewed diff instead of a broken build. `Not yet enforced: #56`.
- **MUST** build test fixtures through one shared builder per type. Why: the same `Issue`,
  `Config` and `Harness` literals repeated across files drift apart the first time a field is
  added. `Not yet enforced: #54`.
- **MUST NOT** run an example from any test or CI step. Why: `examples/broker_live.rs` spawns
  a real `claude` process and spends tokens. Cargo's `test = false` default for examples is the
  only thing that keeps `cargo test` from calling it.

## Comments and docs

- **MUST** write comments that carry the why, usually which failure mode is avoided. The what
  is in the code. Why: a comment that restates the code goes stale on the next edit, and a
  comment that names the failure mode tells the next reader what not to remove. Example: the
  comment on `EXP_CAP` in [src/sched/retry.rs](../src/sched/retry.rs).
- **MUST** open every module with a `//!` doc that states what the module is for and which
  decision it records. Why: the module doc is the one place a reader looks before the code.
  Example: [src/broker/server.rs](../src/broker/server.rs), which records why the transport is
  hand-rolled.
- **SHOULD** link incident history rather than retell it. A comment cites the issue number or
  the ADR, in one line. Why: the same story told in a comment, in a module doc and in CLAUDE.md
  goes out of sync in three places. The `docs/adr/` directory starts with the decision in #57.
- **MUST** match the comment density of the surrounding code. This codebase comments decisions,
  not lines. Why: a block of narrative in a file of terse code is a sign the narrative belongs
  in a doc.
- **MUST** run `cargo fmt` rather than hand-wrapping. The settings are `max_width = 100` and
  `use_small_heuristics = "Max"` in [rustfmt.toml](../rustfmt.toml). Why: `rustfmt` makes
  different choices than you will, and CI checks its choices.

## Dependencies

- **SHOULD** prefer a maintained crate over hand-rolled infrastructure once the hand-rolled
  version passes about fifty lines, unless a module doc records why not. Why: a parser, a
  migration runner or a signal wrapper that the crate ecosystem already provides is code this
  repository then has to test and maintain alone.
- **MUST** justify a new dependency in one paragraph in the PR: what it replaces, why the
  alternatives below do not fit, and what it adds to the dependency tree. Why: the tree is small
  on purpose, and every addition is a permanent maintenance cost.
- **MUST** prefer a crate for platform-specific plumbing over a direct syscall or a device
  file. Why: a `/dev/urandom` read or a raw `libc::kill` works on the developer machine and
  fails silently elsewhere. `Not yet enforced: #51`.
- **MUST** keep `Cargo.lock` committed and build with `--locked` in CI. Why: a drifted lock
  file must fail the build rather than be rewritten quietly.

### Current dependencies

| Crate | Role in this repository | Note |
| :---- | :---- | :---- |
| `anyhow` | Error context at the binary boundary: `main.rs`, `tui`, `api`, `sched`, `project` | Never inside a domain trait |
| `blake3` | Collision-proof suffix for `worktree_key`, derivation of `session_id` | Deterministic on purpose, see `src/model.rs` |
| `clap` | Command line, derive style | |
| `crossterm` | Terminal backend for the TUI | |
| `libc` | Process-group signals in `src/worker/claude.rs` | Leaves with #50 |
| `ratatui` | The dashboard | |
| `rusqlite` (bundled) | The store | Bundled so no system SQLite is needed |
| `serde`, `serde_json` | Config, `stream-json`, MCP framing, GitHub payloads | |
| `thiserror` | Typed errors in library modules | |
| `time` | RFC 3339 parsing of GitHub timestamps | The only date parsing in the crate |
| `tokio` | Ops API listener, `watch`/`mpsc`/`oneshot` channels, the main loop, signals | Not used by the domain traits |
| `toml` | Config file | |
| `tracing`, `tracing-subscriber` | Logs, always to stderr | stdout belongs to the TUI |
| `ureq` | Blocking HTTP client over `rustls` | Chosen for the blocking API and for needing no C toolchain |

### Approved to add

These crates are pre-approved by the quality plan. Adding one needs no justification paragraph,
only the linked issue.

| Crate | Purpose | Issue |
| :---- | :---- | :---- |
| `parking_lot` | Non-poisoning locks, removes every `lock().unwrap()` | #49 |
| `nix` | Safe `kill` to a process group, removes every `unsafe` block | #50 |
| `getrandom` | OS entropy for the broker bearer token | #51 |
| `tempfile` | Temp directories in tests with automatic cleanup | #54 |
| `rstest` | Shared fixtures for the test harness | #54 |
| `insta` | Snapshot tests for rendered text | #56 |
| `tiny_http` | One HTTP/1.1 server for both listeners | #57 or its follow-ups |
| `serde_rusqlite` | Derived row mapping in the store | Quality plan, no issue yet |
| `strum` | Derived enum labels | Quality plan, no issue yet |
| `bon` | Builders for constructors with many parameters | Quality plan, no issue yet |

### Needs an ADR first

`axum`, `hyper`, `rmcp`, `reqwest`, `octocrab`, `sqlx`. Each one changes the sync-versus-async
model or doubles the dependency tree. The decision is #57, and the module docs in
[src/broker/server.rs](../src/broker/server.rs) and [src/api/mod.rs](../src/api/mod.rs) record
why they were rejected so far.

## Concurrency

- **MUST** keep the domain traits blocking: `Tracker`, `TrackerWrites`, `Store`, `Worker`,
  `Workspace` and `Scheduler::tick`. Why: the scheduler tests drive `tick()` directly, and an
  async scheduler would need a runtime in every test.
- **MUST** confine `tokio` to `src/main.rs` and `src/api/`. Why: the runtime exists for the ops
  surface and the main loop. A domain module that imports it has crossed the boundary.
- **MUST NOT** call a blocking operation on a runtime thread. SQLite, `ureq` and `git` go
  through `spawn_blocking` or stay off the runtime. Why: one blocked worker thread delays every
  ops API response that shares the runtime. `Not yet enforced: #55`.
- **MUST** confine `std::thread::spawn` to the two places that drive a blocking resource:
  the reader thread in [src/worker/claude.rs](../src/worker/claude.rs) and the listener in
  [src/broker/server.rs](../src/broker/server.rs). Why: a thread that nothing joins is a
  process that does not exit.
- **MUST NOT** hold a lock across an `.await`. Why: the runtime cannot preempt the holder, and
  every other task that wants the lock stalls.
- **MUST** pass state between the scheduler and its observers through channels, never through
  shared mutable state. Why: `watch` for the snapshot and `oneshot` for a reply are what keep
  a slow client off the tick. Example: `Command` handling in [src/api/mod.rs](../src/api/mod.rs).
- **MUST** read the clock only through the injected `Clock`. Nothing outside
  [src/clock.rs](../src/clock.rs) calls `Instant::now` or `SystemTime::now`. Why: time that
  only moves when a test moves it is what makes the suite deterministic.
- **Open decision.** The crate runs a blocking domain under a `tokio` ops surface. Whether to
  go all-async or drop `tokio` is #57. Until that ADR lands, the rules above describe the
  model as it is.

## Structure

- **MUST** keep to `clippy::all` at default thresholds. `too_many_arguments` at 7 is the only
  numeric size limit. Why: the team agreed on the default lint set, not on a bespoke one, so
  that the bar is the same in every editor and on CI.
- **MUST** give each concern its own file and its own `impl` block. A new `Scheduler` concern
  goes into a submodule under `src/sched/` with its own `impl Scheduler`, not into the existing
  block. Why: multiple `impl` blocks keep `self` in reach without threading the whole
  scheduler through a free function.
- **MUST** make observers share a formatter, never each other. `fmt_count`, `fmt_ms` and
  `Phase::label` belong in a shared module, and `api/render.rs` must not import from `tui`.
  Why: the TUI, the ops API and the status client are peers over one `Snapshot`.
  `Not yet enforced: #56`.
- **SHOULD** extract a function when a reader must scroll to see all of it. Why: `main.rs`
  and `Scheduler::launch` show the cost of not doing so. `clippy::all` does not enforce this,
  so the reviewer does.
- **MAY** use let-chains (`if x && let Some(y) = z`). The crate is Rust edition 2024. Why: they
  remove a level of nesting and are already used throughout.

## Lints and commit gate

- **MUST** run, before every commit, in this order:
  1. `cargo fmt`
  2. `cargo clippy --fix --all-targets --allow-dirty`
  3. `cargo clippy --all-targets -- -D warnings`
  4. `cargo test`
  Why: CI runs steps 1, 3 and 4 as checks, and a commit that fails any of them fails the PR.
  Step 2 is local only, because CI cannot commit what `--fix` changes.
- **MUST** treat every warning as an error. `clippy::all` at default parameters, no
  `pedantic` group. Why: a warning that is allowed to stay becomes twenty.
- **MUST** declare the lint set in the `[lints]` table of `Cargo.toml`, so that a local
  `cargo clippy` and rust-analyzer report the same errors CI does. `Not yet enforced: #52`.
- **MUST** keep the crate free of `unsafe`, with `unsafe_code = "forbid"` in `[lints.rust]`.
  Why: the only `unsafe` today is `libc::kill`, and `nix` provides the same call safely.
  `Not yet enforced: #50`.
- **MUST** run `cargo deny check` and `cargo machete` in CI, and run the tests through
  `cargo nextest` for per-test timeouts. Why: a RUSTSEC advisory or an unused dependency has no
  other signal. `Not yet enforced: #53`.
- **MUST** keep the toolchain pinned in [rust-toolchain.toml](../rust-toolchain.toml). Why:
  "it compiles" must mean the same thing on CI, on the developer machine and in every
  dispatched worktree.

## Visibility

- **MUST** default to `pub(crate)`. An item is `pub` only when `tests/` or `examples/` reaches
  it. Why: the library exists to back one binary and one test suite, and a public item is a
  promise to a consumer that does not exist. `Not yet enforced: #58`.
- **MUST** give an observer a `Snapshot` and never a `Store`. Why: rule 3 in CLAUDE.md is
  enforced by the type when the observer's struct cannot hold a `Store`. Example: `Api` in
  [src/api/mod.rs](../src/api/mod.rs) holds a `watch::Receiver` and a `Command` sender.
- **MUST** add a field to `Snapshot` when an observer needs data it does not carry, rather
  than giving the observer a new source. Why: `Row.branch` was added for the status client
  instead of letting the client ask `git`, and the same choice applies next time.
