# crewd

**Point it at your backlog. It works the tickets and hands you pull requests.**

crewd is a daemon that polls an issue tracker, opens a git worktree per issue, runs a coding
agent in it, and reconciles what comes back. You review pull requests. Nothing it does merges.

> The crate and binary are still named `symphony-cc`. Renaming them to `crewd` (the daemon)
> and `crewctl` (the client) is tracked in
> [#45](https://github.com/StGerman/crewd/issues/45); the repository has been renamed already.

## Why this exists

Running one coding agent is easy. Running several, unattended, against a real backlog is where
it falls apart — and it falls apart in specific, repeatable ways:

- **An agent that says "done" is not done.** Two branches cut from the same base can each pass
  the test suite alone and fail together. crewd rebases a finished branch onto the current base
  and re-runs your checks *before* it believes the verdict. A failure goes back to the agent
  with the output in hand.
- **A loop with no brake spends your whole budget.** Continuation, retry, review round-trips
  and tool calls are each bounded, per session *and* per issue, and the per-issue bounds never
  reset. An agent cannot buy itself a fresh budget by starting a new session.
- **A crash should cost a poll, not a wedged queue.** The database is a cache of judgment, not
  a system of record. Losing it degrades to re-polling the tracker. A hard kill releases its
  claims at the next startup rather than leaving an issue marked running forever.
- **You should be able to see what happened.** Every run writes its full event stream to a
  transcript on disk, and a running daemon answers questions over an HTTP API without being
  disturbed.

The design stance is that the orchestrator is the only authority and every external thing —
tracker, agent, git, clock — sits behind a seam with a fake behind it. That is why the test
suite has no sleeps in it, and why "would this still be correct if the agent were working on
this repo?" is a question the code can actually answer.

**Nothing merges without a human.** The trait that talks to the forge has no merge method, and
is not going to grow one.

## Installation

You need Rust, `git`, and the [Claude Code CLI](https://claude.com/claude-code) logged in.

```bash
git clone https://github.com/StGerman/crewd.git
cd crewd
cargo build
cargo run -- --tui
```

That last command runs against fake demo data — no tracker is contacted and no agent is
spawned, so it is safe to explore. `q` quits.

### Pointing it at your own repository

1. **Copy `symphony.github.toml`** and set `tracker.owner` / `tracker.repo` to yours. Both
   shipped configs are heavily commented; the comments explain why each number is what it is,
   which is usually more useful than the number.
2. **Label the issues you want worked.** `tracker.required_labels` is the filter — `agent` by
   default. An issue without the label is invisible to the daemon.
3. **Give it a token.** `GITHUB_TOKEN` in the environment, never in the file.
4. **Decide whether it acts.** `tracker.kind = "github"` decides what it *looks at*;
   `worker.kind = "claude"` decides whether it *works*. Leaving the worker fake is a safe way
   to watch real dispatch decisions before you let anything edit code.
5. **Decide whether it publishes.** `[delivery]` pushes branches and opens pull requests under
   your credentials. Off by default.

Steps 4 and 5 are separate switches on purpose. Turning this from "watch my backlog" into
"work my backlog" should be a decision you make twice.

### Giving it its own identity

By default every comment, label and branch is authored by *you*, because the token is yours. A
GitHub App gives the daemon its own identity, so its writes are distinguishable from yours and
it can hold `contents: write` while being denied merge entirely.

This is **not wired up yet** — the config keys land with
[#64](https://github.com/StGerman/crewd/issues/64), and a one-click `init` that registers the
App for you is [#65](https://github.com/StGerman/crewd/issues/65). `GITHUB_TOKEN` is the
supported path today and will stay supported.

Two findings from setting one up by hand, since they shape how dispatch works: an App's
`[bot]` account **cannot be an issue assignee** outside GitHub's partner agent program, and a
separate marker account does not help because a collaborator on a personal repository cannot be
given read-only access. That is why the issues crewd picks up are marked by a **label** rather
than by assignment.

### Requirements, in full

| | |
|---|---|
| Rust | pinned in `rust-toolchain.toml`; only rustup reads it |
| `git` | real worktrees are created on disk |
| `claude` | logged in; needed only once `worker.kind = "claude"` |
| Memory | budget 1–2 GB per concurrent agent |
| Tokens | the real limit. Five concurrent agents can exhaust a five-hour window in minutes |

## Contributing

**Work is picked up from GitHub Issues on this repository, labelled `agent`** — not from a plan
file. That is the same mechanism described above, pointed at itself: crewd's own backlog is
worked by crewd.

Because of that, most development here is done by an agent rather than by a person at a
keyboard, and the documentation is split to match:

- **[CLAUDE.md](CLAUDE.md)** is the working surface — every command, the architecture, the
  invariant table, and the traps that are expensive to rediscover. Read it before changing
  anything under `src/sched/`.
- **[docs/coding-guidelines.md](docs/coding-guidelines.md)** is how code is written here, with
  a named check for every rule. Read it before your first edit.

The commit gate is `cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check`. CI runs all three on every push and pull request.

If you are opening an issue you would like an agent to work, it needs the `agent` label and
enough context to act on — the invariant table in CLAUDE.md is the standard the codebase holds
itself to, and an issue that names which invariant is at stake is one an agent can finish.

## Status

Slices 1–6 are complete: a deterministic core with a fake behind every seam, real git
worktrees, a real GitHub Issues tracker, a real `claude -p` worker, a host-side tool broker
that lets an agent write to its own ticket without ever holding the credential, an ops API with
a status client and an MCP server over it, a handoff gate, and delivery through to pull
requests and review round-trips.

It is a Rust reimplementation of the coordination layer in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`, written after a review found
several concrete defects in that design. Most of the invariant table exists to not have them.
