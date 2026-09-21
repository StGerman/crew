# crew

A tracker-driven orchestrator for Claude Code agents. It polls an issue tracker, opens a git
worktree per issue, runs a coding-agent session in it, rebases and re-runs your checks before
it believes the agent, and opens a pull request for you to review.

Nothing it does merges. That is not a setting.

> **Naming.** The crate and binary are still `symphony-cc`; the repository is `crew` and the
> rename to `crewd` (daemon) and `crewctl` (client) is tracked in
> [#45](https://github.com/StGerman/crew/issues/45). Commands below use the name that works
> today.

## What you need

- **Rust** — the toolchain is pinned in `rust-toolchain.toml` and only rustup reads it. A
  Homebrew `rust` works but ignores the pin.
- **`claude`** — the Claude Code CLI, logged in. Only needed once you switch the worker on;
  the default config runs a fake worker and needs nothing.
- **`git`** — real worktrees are created on disk.
- **rust-analyzer** (optional) — dispatched agents navigate this repo over LSP rather than
  grep. See [Setting up rust-analyzer](#setting-up-rust-analyzer).

```bash
git clone https://github.com/StGerman/crew.git
cd crew
cargo build
```

## Quickstart

Everything below runs against fake demo data. No tracker is contacted, no agent is spawned,
and nothing is written to a real repository.

```bash
cargo run -- --tui                    # dashboard against the fake tracker
cargo run -- --max-ticks 20           # headless: run 20 ticks, then exit
cargo run --example dashboard_preview # render the UI to stdout, no terminal needed
```

In the dashboard, `r` refreshes and `u` clears a quarantine. Headless is the default and
`--tui` opts in, so the dashboard never becomes something the daemon depends on.

Point the store and the task projection somewhere disposable before any run whose side
effects you do not want to keep:

```bash
SYMPHONY_DB=/tmp/x.db SYMPHONY_TASKS_ROOT=/tmp/tasks cargo run -- --max-ticks 20
```

Without `SYMPHONY_TASKS_ROOT`, a smoke run's demo issues (`iss-001`, `MT-601`, …) land in your
real Claude Code task list.

## Watching a run

```bash
cargo run -- --api 127.0.0.1:8787     # start the daemon with the ops API on for this run
cargo run -- status                   # what it is doing, read over that API
cargo run -- status 64                # one issue in full: phase, attempt, turns, cost, branch
```

`status` is a client and nothing else — it holds no database handle, so asking what is running
cannot disturb it. If it says nothing is listening, the likeliest reading is that the API is
off, which is the default.

Every run also writes its raw event stream to `<workspace.root>/.transcripts/<run>.jsonl`, and
the `dispatched` log line names the file. That is the first thing to reach for when you want to
know what a run actually did:

```bash
jq -c 'select(.type=="assistant")' <file>   # the turns
grep symphony_run_end <file>                # how it exited
```

Logs go to stderr — always, because under `--tui` the alternate screen owns stdout.
`RUST_LOG=symphony_cc=debug` raises the level.

## Running against real GitHub Issues

`symphony.github.toml` is checked in and ready to use. It contains no token.

```bash
GITHUB_TOKEN=$(gh auth token) cargo run -- --config symphony.github.toml --max-ticks 3
```

That lists this repository's open, `agent`-labelled issues and dispatches none of them, because
`worker.kind` is still `"fake"` in that file.

**Two switches, deliberately separate.** `tracker.kind = "github"` decides what the daemon
*looks at*; `worker.kind = "claude"` decides whether it *acts*. A real tracker with the fake
worker is a safe way to watch real dispatch decisions without spawning anything. Flipping
`worker.kind` to `"claude"` is what turns "list this backlog" into "work it" — and then
`--max-ticks` spawns real `claude -p` processes with `--permission-mode bypassPermissions`
against real worktrees. It is not a dry run.

`[delivery]` is the third switch. With it on, a finished branch is pushed and a pull request
opened **under your credentials**. It is off by default for that reason.

### Do not run the daemon from inside a worktree

`GitWorktreeWorkspace::new` refuses at startup if `workspace.repo` or `workspace.root` resolves
inside a linked worktree, because the worktrees it would create register in the top-level
checkout's `.git` where no orchestrator recorded them. The error names the way out: point
`workspace.repo` at a throwaway clone and `workspace.root` beside it. Setting `SYMPHONY_DB`
does not help — the litter was never in the database.

## Giving the orchestrator its own identity

By default every write — comments, labels, branches, pull requests — is authored by *you*,
because `GITHUB_TOKEN` is your personal token. A GitHub App gives the daemon its own identity,
so its writes are distinguishable from yours in GitHub's audit log, and it can be granted
`contents: write` while being denied merge.

**This is not wired up yet.** The App exists and is installed; the config keys that would make
the daemon use it land with [#64](https://github.com/StGerman/crew/issues/64), and a one-click
`init` that registers the App for you is [#65](https://github.com/StGerman/crew/issues/65).
Until then, `GITHUB_TOKEN` is the supported path and will stay supported.

To register an App by hand today, there is a wizard:

```bash
bash scripts/setup-github-app.sh
```

It walks five stages: register the App, place the private key outside the repository, install
it, verify a real installation token, and create the dispatch label. The permissions it asks
for are exactly:

| Permission | Level | Why |
|---|---|---|
| Contents | Read and write | push branches |
| Issues | Read and write | comments, `state:*` labels |
| Pull requests | Read and write | open PRs, reply on review threads |
| Commit statuses | Read-only | delivery reads CI |
| Checks | Read-only | delivery reads CI |
| Metadata | Read-only | mandatory |

No merge permission, deliberately.

Two things worth knowing before you set one up. An App's `[bot]` account **cannot be an issue
assignee** outside GitHub's partner agent program, so dispatch is marked by a **label**, not by
assignment — and a separate marker account does not fix it, because a collaborator on a
personal repository cannot be given read-only access at all. And the private key belongs to
whoever owns the App, so every operator registers their own; there is no shared Crew App to
install.

## Supervising with an agent

The same four ops routes are available as MCP tools, for an agent watching the daemon:

```bash
cargo run -- --mcp 127.0.0.1:8788
claude mcp add --scope local --transport http crew_ops http://127.0.0.1:8788/ops
```

**Local or project scope, never user scope.** A user-scope server is inherited by every
dispatched worker, and a worker that could call `unquarantine` could clear its own quarantine
and re-dispatch itself. The wiring keeps these tools away from workers; user scope is the one
way to defeat it by configuration.

## Configuration

Two configs are checked in:

| File | Tracker | Worker | For |
|---|---|---|---|
| `symphony.toml` | fake | fake | the quickstart; no side effects |
| `symphony.github.toml` | GitHub | fake | this repository's real backlog |

Both are heavily commented — the comments explain why each number is what it is, which is
usually more useful than the number. Sections worth knowing: `[agent]` (concurrency and turn
budgets), `[gate]` (what must pass before a branch is handed over), `[delivery]` (pushing and
pull requests), `[broker]` (the agent's scoped write tools), `[transcripts]`, `[api]`.

Environment overrides:

| Variable | Effect |
|---|---|
| `SYMPHONY_DB` | where the store lives |
| `SYMPHONY_TASKS_ROOT` | where the `~/.claude/tasks` projection is written |
| `RUST_LOG` | log level, e.g. `symphony_cc=debug` |
| `GITHUB_TOKEN` | required when `tracker.kind = "github"` |

Sizing `agent.max_concurrent`: each dispatched agent brings its own rust-analyzer index and its
own `cargo check`, so budget 1–2 GB resident apiece. Token burn binds sooner than memory — five
concurrent agents drove a five-hour window to exhaustion in minutes.

## Development

```bash
cargo test                                 # 226 unit + 90 integration
cargo clippy --all-targets -- -D warnings  # the standing bar is zero warnings
cargo fmt --check
```

Those three are the commit gate, and CI runs them on every push and pull request.

```bash
cargo test --lib                # unit only
cargo test --test scheduler     # scheduler integration only
cargo test --test api           # ops API integration only
cargo test a_permanent_failure  # one test; the argument is a substring match
```

`cargo run --example broker_live` drives a real `claude` against a real broker. It needs a
working login and **spends tokens**, so it is never run by CI or by `cargo test`.

Read [CLAUDE.md](CLAUDE.md) for the architecture and the invariant table, and
[docs/coding-guidelines.md](docs/coding-guidelines.md) before your first edit.

### Setting up rust-analyzer

[.mcp.json](.mcp.json) hands agents rust-analyzer over MCP. It needs three pieces on the host:
the `rust-analyzer` server, `rust-src`, and the `rust-analyzer-mcp` bridge. How you install the
first two depends on whether your toolchain came from rustup or Homebrew, so the steps live in
the `setup-rust-analyzer` skill rather than here. A `SessionStart` hook probes for all three and
names whichever is missing — without it the only symptom is an ENOENT at connect time, which
tells you nothing about which piece to install.

## Troubleshooting

**Linking fails, or `git` refuses everything with "You have not agreed to the Xcode license
agreements".** The problem is which developer directory is active, not your code and not a
missing toolchain:

```bash
xcode-select -p    # /Applications/Xcode.app means an unaccepted license
sudo xcode-select --switch /Library/Developer/CommandLineTools
```

`xcode-select --install` does **not** fix this. It installs the Command Line Tools; it does not
make them active, so the symptom is unchanged and the real cause stays hidden.

**`status` says nothing is listening.** The ops API is off by default. Start the daemon with
`--api <addr>` or set `[api] enabled` in the config. The error distinguishes "no daemon" from
"a daemon that refused the request" on purpose — check which one you got before restarting
anything.

**An issue is quarantined and will not dispatch.** Clear it with `u` in the dashboard or
`POST /api/v1/unquarantine/<id>`. Quarantine is not a bug; it is what stops a permanently
failing issue from retrying forever.
