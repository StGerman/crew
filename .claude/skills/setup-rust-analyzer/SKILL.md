---
name: setup-rust-analyzer
description: Install and verify this repo's rust-analyzer MCP toolchain — the rust-analyzer language server, rust-src, and the rust-analyzer-mcp bridge that .mcp.json spawns. Use when the rust-analyzer MCP server fails to connect (ENOENT on rust-analyzer-mcp), when the SessionStart hook reports a missing piece, when LSP navigation tools are unavailable in this repo, or when setting the repo up on a new machine.
---

# Setting up rust-analyzer MCP

[.mcp.json](../../../.mcp.json) spawns `$HOME/.cargo/bin/rust-analyzer-mcp`, which in turn
spawns `rust-analyzer` from `PATH`. Three pieces have to be present; the SessionStart hook
([.claude/hooks/rust-analyzer-check.sh](../../hooks/rust-analyzer-check.sh)) names whichever
are missing.

## 1. Identify the toolchain flavor first

The install commands differ, and the wrong one fails confusingly:

```bash
command -v rustup cargo rustc
```

- **rustup present** — components come from rustup.
- **cargo/rustc present, no rustup** (e.g. `brew install rust`) — Homebrew's `rust` formula
  ships `rust-src` but **not** `rust-analyzer`, and `rustup component add` does not exist.
  This is the case that makes the CLAUDE.md quickstart wrong if followed literally.
- **neither** — install a toolchain first (`brew install rustup-init && rustup-init`, or
  rustup.rs).

## 2. Install the missing pieces

`rust-analyzer` — the language server:

```bash
rustup component add rust-analyzer   # rustup toolchains
brew install rust-analyzer           # Homebrew toolchains (separate formula from `rust`)
```

`rust-src` — std sources. Without them `hover`/`definition` into `std` answer blank:

```bash
rustup component add rust-src        # rustup only; Homebrew's `rust` already includes it
```

Verify either way — this path must exist:

```bash
ls "$(rustc --print sysroot)/lib/rustlib/src/rust"
```

`rust-analyzer-mcp` — the MCP bridge. Always via cargo, on every flavor. It compiles, so
expect roughly a minute:

```bash
cargo install rust-analyzer-mcp
```

It lands in `$HOME/.cargo/bin/` regardless of where the toolchain came from, which is the
path `.mcp.json` names. Cargo warns that `~/.cargo/bin` is not on `PATH` on a Homebrew
machine — harmless here, since `.mcp.json` uses the absolute path.

## 3. Verify before handing back

Re-run the hook script; silence and exit 0 means all three are present:

```bash
.claude/hooks/rust-analyzer-check.sh; echo "exit=$?"
```

Then prove the server actually speaks MCP in this workspace, rather than trusting that the
binaries exist:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | "$HOME/.cargo/bin/rust-analyzer-mcp" . | head -c 400
```

A `serverInfo` result followed by a tool list is the pass condition.

## 4. Tell the user to reconnect

The MCP client does not retry on its own. Finish by telling them to run `/mcp` — the
connection is only live once that reports success. On a machine that has never used this
repo, `/mcp` also asks them to trust the project's `.mcp.json` server; that approval is
per-machine and deliberately not committed, so it cannot be automated away here.

## Known traps

- **`${HOME}` in .mcp.json** expands at connect time. If a session still reports ENOENT on a
  literal `${HOME}/.cargo/...` after the binary exists, that client is not expanding it —
  substitute the absolute path rather than reinstalling.
- **First-load emptiness.** Once connected, `references`/`definition`/`hover` answer from
  whatever is indexed so far and return empty during the initial `cargo check`. Empty is
  indistinguishable from "no callers" — ask again until an answer is non-empty before
  concluding anything.
- **Xcode license.** On macOS, an install that dies at the linker is usually
  `xcode-select -p` pointing at Xcode.app rather than CommandLineTools. See Troubleshooting
  in [CLAUDE.md](../../../CLAUDE.md).
