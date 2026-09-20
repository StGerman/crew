#!/usr/bin/env bash
# SessionStart probe for the rust-analyzer MCP toolchain.
#
# .mcp.json spawns $HOME/.cargo/bin/rust-analyzer-mcp directly, so a machine that
# never ran the setup gets a bare ENOENT at connect time with no hint of which of
# the three pieces is missing. This reports that up front. It never installs and
# never fails the session — a missing toolchain degrades navigation to grep, it
# does not stop work.
set -uo pipefail

missing=()

[ -x "$HOME/.cargo/bin/rust-analyzer-mcp" ] ||
  missing+=("rust-analyzer-mcp — the MCP bridge .mcp.json spawns")

command -v rust-analyzer >/dev/null 2>&1 ||
  missing+=("rust-analyzer — the language server itself")

if command -v rustc >/dev/null 2>&1; then
  sysroot=$(rustc --print sysroot 2>/dev/null || true)
  if [ -z "$sysroot" ] || [ ! -d "$sysroot/lib/rustlib/src/rust" ]; then
    missing+=("rust-src — std sources; without them std navigation answers blank")
  fi
else
  missing+=("a Rust toolchain — rustc is not on PATH")
fi

[ ${#missing[@]} -eq 0 ] && exit 0

list=""
for item in "${missing[@]}"; do
  list="$list\\n  - $item"
done

printf '{"systemMessage":"rust-analyzer MCP is not set up on this machine:%s\\n\\nRun /setup-rust-analyzer to install it, then /mcp to reconnect. Until then, navigation in this repo is grep rather than LSP."}\n' "$list"
