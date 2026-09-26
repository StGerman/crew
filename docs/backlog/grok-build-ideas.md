# grok-build → crewd: idea backlog (untriaged)

Features, ideas, and patterns found in [grok-build](https://github.com/xai-org/grok-build) that crewd could copy.
**This list is untriaged.** It makes no fit assessment and no comparison with crewd. Triage, grooming, and prioritization happen in separate sessions.

- Source: grok-build @ `f0e3be1100ef5252488e3be8bb0e91cf68d8c305` (upstream `SOURCE_REV`: `036a5d8348cd744767cd0b08518ab17bf608fa7f`), harvested 2026-09-26
- Paths are relative to the grok-build repo root. `…/user-guide/` = `crates/codegen/xai-grok-pager/docs/user-guide/`
- IDs (`GB-nn`) are stable handles for triage. Don't renumber; strike or annotate instead.

## 1. Orchestration & lifecycle

### Agent & session lifecycle
- [ ] `GB-01` **Lifecycle hooks as data**: session start/end and turn start/done/abort/error are sent as plain data events to registered contributors. `crates/codegen/xai-agent-lifecycle/src/lib.rs`
- [ ] `GB-02` **Active sessions registry**: a lock-guarded JSON file of live session PIDs; dead PIDs are pruned with a signal probe. `crates/codegen/xai-grok-active-sessions/src/lib.rs`
- [ ] `GB-03` **Foreign sessions discovery**: a bounded, metadata-only listing of other agents' sessions, read from their own SQLite stores (30-day cap, per-tool limits). `crates/codegen/xai-grok-foreign-sessions/src/lib.rs`
- [ ] `GB-04` **Subagent resolution**: works out a subagent's definition, runtime prompt, and resume state for multi-agent runs. `crates/codegen/xai-grok-subagent-resolution/`
- [ ] `GB-05` **Session ops (resume / fork / rewind / rename / compact)**: sessions are persisted, first-class objects, and each operation has a CLI and a UI. `…/user-guide/17-sessions.md`
- [ ] `GB-06` **Background tasks, `/loop`, monitor, scheduler**: long-running commands detach from the conversation, and a scheduler plus a monitor tool re-run or watch them. `…/user-guide/20-background-tasks.md`

### Headless / automation
- [ ] `GB-07` **Machine-readable output formats**: plain, json, streaming-json, and streaming-messages-json. `…/user-guide/14-headless-mode.md`
- [ ] `GB-08` **Per-run tool filtering and `--allow`/`--deny` rules**: permission rules that apply to a single invocation. `…/user-guide/14-headless-mode.md`
- [ ] `GB-09` **Named and resumable headless sessions**: `-s` names a session, `-r` resumes, `-c` continues, and the session ID can be read from the output. `…/user-guide/14-headless-mode.md`

### Worktrees & git
- [ ] `GB-10` **Fast worktree provisioning**: BTRFS snapshots and parallel copy-on-write clones make worktrees near-instant. Metadata lives in SQLite, and an auto-GC cleans up. `crates/codegen/xai-fast-worktree/src/lib.rs`, `auto_gc.rs`
- [ ] `GB-11` **Pre-created worktree pool + sync**: linked worktrees are synced to a target with gix HEAD resolution and git CLI changes, using reflink copies. `crates/codegen/xai-fast-worktree/src/sync.rs`
- [ ] `GB-12` **Parallel git status with a thread cap**: gix-based status runs on at most 8 workers and leaves headroom for other threads. `crates/codegen/xai-gix-status/src/lib.rs`
- [ ] `GB-13` **Hunk attribution**: each hunk is tagged as an agent edit or an external edit by a dedicated actor task, and the result is streamed out. `crates/codegen/xai-hunk-tracker/src/lib.rs`
- [ ] `GB-14` **Projected clone**: the repo goes into a content store and is mounted as a projected working tree (NFS on macOS, FUSE on Linux) instead of a full checkout. `…/user-guide/27-grok-clone.md`

### Workflows
- [ ] `GB-15` **Scripted workflow engine**: orchestration written in Rhai scripts, with parallel agent fan-out (up to 1024). `crates/codegen/xai-workflow/src/engine.rs`
- [ ] `GB-16` **Journaled replay-resume**: host calls are stored in a hash-keyed journal with a size budget (1 MB default, 64 MB max), so a crashed workflow replays up to where it stopped. `crates/codegen/xai-workflow/src/journal.rs`

### Hooks & permissions
- [ ] `GB-17` **File-based hook discovery**: hooks live in global and per-worktree directories and are snapshotted and indexed by event type. `crates/codegen/xai-grok-hooks/src/discovery.rs`
- [ ] `GB-18` **Hook verdicts (permit / deny / rewrite)**: a hook can block or rewrite its input, and a hook error fails open. `crates/codegen/xai-grok-hooks/src/dispatcher.rs`
- [ ] `GB-19` **Command and HTTP hook runners**: a hook runs as a sandboxed subprocess or as an HTTP call, under a network policy. `crates/codegen/xai-grok-hooks/src/runner/mod.rs`
- [ ] `GB-20` **Remembered permission grants**: per-tool "always allow" choices are remembered per project, with preselected defaults. `…/user-guide/22-permissions-and-safety.md`

### Sandboxing
- [ ] `GB-21` **OS-level sandbox**: Landlock on Linux and Seatbelt on macOS (via nono), with read-only, read-write, and deny path sets, and network blocking for child processes. `crates/codegen/xai-grok-sandbox/src/lib.rs`
- [ ] `GB-22` **Sandbox profiles**: built-in profiles (workspace, devbox, read-only, strict, off) plus custom TOML profiles that support `extends` and deny globs. `crates/codegen/xai-grok-sandbox/src/profiles.rs`
- [ ] `GB-23` **Protect the control plane**: the agent cannot write to global hook sources, so it cannot rewrite its own guardrails. `crates/codegen/xai-grok-sandbox/src/lib.rs`

### Daemon & process lifecycle
- [ ] `GB-24` **Self-daemonization**: double-fork and `setsid` before the async runtime starts, with a single-instance pidfile lock. `crates/codegen/xai-grok-workspace-daemon/src/daemonize.rs`
- [ ] `GB-25` **Graceful drain on SIGTERM**: a two-phase drain (stop RPC, then flush the upload queue) within a time budget, with a "draining" marker file and pidfile takeover. `crates/codegen/xai-grok-workspace/src/handle.rs`
- [ ] `GB-26` **Sleep/wake awareness**: suspend and resume notifications on macOS, Linux (DBus), and Windows, used to defer or re-arm work. `crates/codegen/xai-system-power/src/lib.rs`
- [ ] `GB-27` **Activity windows / idle hold**: per-connection activity windows decay over time, so the process doesn't look idle while it's being used. `crates/codegen/xai-grok-workspace/src/activity.rs`
- [ ] `GB-28` **TTY safety for child processes**: detaches from the controlling TTY, suppresses pagers, and manages process groups. `crates/codegen/xai-tty-utils/src/lib.rs`
- [ ] `GB-29` **Process resource sampling**: CPU and memory sampling of child processes. `crates/codegen/xai-tty-utils/src/process_resources.rs`

### Concurrency & resilience
- [ ] `GB-30` **Bounded advisory file locks**: a non-blocking acquire with a short grace period, so NFS can't wedge the process. `crates/codegen/xai-grok-file-lock/src/lib.rs`
- [ ] `GB-31` **Atomic registry read-modify-write**: writes a tmp file and renames it, with a 2 s lock timeout and a non-blocking unregister that is safe in signal handlers. `crates/codegen/xai-grok-active-sessions/src/lib.rs`
- [ ] `GB-32` **Circuit breaker**: a sliding window with a minimum sample count, open/half-open/closed states, and helpers that classify HTTP and gRPC errors. `crates/common/xai-circuit-breaker/src/lib.rs`
- [ ] `GB-33` **Pinned wire conventions**: adjacent-tagged serde enums, snake_case fields, and `u64` integers keep the API format stable. `crates/codegen/xai-grok-workspace-types/src/lib.rs`

## 2. Observability & ops

### Events, transcripts, search
- [ ] `GB-34` **Versioned JSONL event log**: typed events with RFC 3339 timestamps and a schema version, with a single warning when the log file fails to open. `crates/codegen/xai-grok-session-events/src/log.rs`
- [ ] `GB-35` **Transcript detail levels + index**: verbose, balanced, minimal, or none, with byte caps per segment and an `INDEX.md` that lists the segments. `crates/codegen/xai-compaction-transcript/src/lib.rs`
- [ ] `GB-36` **Full-text session search**: SQLite FTS5 with BM25 ranking, incremental upserts keyed by blake3 content hash, and quarantine plus rebuild on corruption. `crates/codegen/xai-grok-session-search/src/fts.rs`
- [ ] `GB-37` **Lease-fenced bootstrap**: the claim is written as `"{unix_secs}:{owner_token}"` so only its owner can release it, and a schema bump forces a rebuild. `crates/codegen/xai-grok-session-search/src/fts.rs`

### Telemetry & monitoring
- [ ] `GB-38` **Single telemetry client**: product events, error reports, OTel traces, and the unified log all go through one client, with sampling gates. `crates/codegen/xai-grok-telemetry/src/lib.rs`
- [ ] `GB-39` **Documented, versioned OTel schema**: metric names, OTLP log events, resource attributes, a privacy model, and an example collector config are all documented. `…/user-guide/24-monitoring-usage.md`
- [ ] `GB-40` **Span redaction layer**: a pluggable redactor runs over every span batch before export. `crates/codegen/xai-grok-otel/src/provider.rs`
- [ ] `GB-41` **Error-report scrubbing**: each event is redacted before sending, with a low sample rate, PII off, and an environment tag. `crates/codegen/xai-grok-telemetry/src/sentry.rs`
- [ ] `GB-42` **Diagnostics endpoints**: `/ready`, `/statusz` (version and PID), and `/logs` (a byte-capped tail), served over a unix socket or loopback TCP. `crates/codegen/xai-grok-diag-server/src/lib.rs`
- [ ] `GB-43` **Timing and log macros**: timestamped print macros, plus scoped execution timing that feeds into tracing. `crates/codegen/xai-tracing-macros/src/lib.rs`

### Crash safety & storage
- [ ] `GB-44` **Crash handler + minidump**: catches SIGSEGV, SIGBUS, and SIGABRT, writes a compact dump (PC, frame-pointer chain, version), and detects the crash on the next startup. `crates/codegen/xai-crash-handler/src/format.rs`
- [ ] `GB-45` **Filesystem-aware SQLite journal mode**: WAL on local disks, TRUNCATE on network mounts, and a per-host DB suffix to avoid NFS recovery races. `crates/codegen/xai-sqlite-journal/src/lib.rs`
- [ ] `GB-46` **Capped support bundle**: a tar.gz with total and per-file byte caps that tolerates files still being written. `crates/codegen/xai-grok-feedback/src/feedback_archive.rs`

### Security plumbing
- [ ] `GB-47` **Secret sanitizer**: regex detection of API keys, AWS keys, GitHub/GitLab/Slack tokens, PEM keys, JWTs, bearer tokens, and secrets in `key=value` form. `crates/codegen/xai-grok-secrets/src/sanitizer.rs`
- [ ] `GB-48` **URL param redaction**: strips `access_token`, `api_key`, `password`, `state`, and similar parameters from logged URLs. `crates/codegen/xai-grok-secrets/src/sanitizer.rs`
- [ ] `GB-49` **Extra CA bundle**: rustls with the OS and Mozilla root stores plus an extra CA bundle from an env var (for corporate proxies), tolerating load errors. `crates/codegen/xai-grok-extra-ca/src/lib.rs`
- [ ] `GB-50` **Auth behind traits**: credential providers sit behind traits, with bearer-token resolution that handles refresh. `crates/codegen/xai-grok-auth/src/lib.rs`
- [ ] `GB-51` **Shared HTTP clients + timeouts per concern**: clients are built once (`OnceLock`), with separate timeouts for auth, fetch, and settings. `crates/codegen/xai-grok-http/src/lib.rs`

### Distribution
- [ ] `GB-52` **Self-update**: version check, release channels, a progress bar, and installer detection so it prints the right reinstall hint. `crates/codegen/xai-grok-update/src/auto_update.rs`

## 3. UX, TUI & CLI

### Dashboard & TUI
- [ ] `GB-53` **Agent dashboard**: every session grouped by state, with a peek panel, reply, attach, pin, rename, stop, dispatch-new-agent, `Ctrl+/` filter, and persistence. `…/user-guide/23-dashboard.md`
- [ ] `GB-54` **Streaming markdown with checkpoints**: only the tail after the last stable boundary is re-rendered, with syntax highlighting. `crates/codegen/xai-grok-markdown/src/lib.rs`
- [ ] `GB-55` **Terminal color downgrade**: adapts to 256- and 16-color terminals, with highlighting that stays readable on light and dark backgrounds. `crates/codegen/xai-grok-markdown/src/colors.rs`
- [ ] `GB-56` **Scrollback-native minimal mode**: finished blocks go into the terminal's own scrollback, and a live region stays pinned below. `crates/codegen/xai-grok-pager-minimal/src/lib.rs`
- [ ] `GB-57` **Pluggable renderers via a fn-pointer seam**: avoids crate cycles, and the binary installs the renderer at startup. `crates/codegen/xai-grok-pager/src/minimal/api.rs`
- [ ] `GB-58` **Diff hunk view**: edits are turned into tagged hunks with context lines. `crates/codegen/xai-grok-pager-diff/src/lib.rs`
- [ ] `GB-59` **Inline ANSI output widget**: shows live ANSI process output inside the TUI and handles resize and wrapping. `crates/codegen/xai-ratatui-inline/src/lib.rs`
- [ ] `GB-60` **TextArea widget**: an undo-aware buffer, a clipboard trait, and word-by-word navigation. `crates/codegen/xai-ratatui-textarea/src/lib.rs`
- [ ] `GB-61` **Themes with live preview**: auto, light, and dark, applied live while browsing and saved only on confirm. `crates/codegen/xai-grok-pager/src/slash/commands/theme.rs`
- [ ] `GB-62` **Mermaid rendering**: mermaid → SVG → raster in pure Rust, run in a subprocess with a wall-clock timeout. `crates/codegen/xai-grok-mermaid/src/lib.rs`
- [ ] `GB-63` **Customizable status line**: built-in items or the output of a custom command. `crates/codegen/xai-grok-status-line/src/config.rs`

### Commands & settings
- [ ] `GB-64` **Fuzzy command palette**: MRU ranking, a grouped menu, and a suggestion dropdown. `crates/codegen/xai-grok-pager/src/slash/mod.rs`
- [ ] `GB-65` **Command registry with provenance**: tracks whether each command is built in, from a skill, or from the shell, and supports aliases. `crates/codegen/xai-grok-pager/src/slash/registry.rs`
- [ ] `GB-66` **Settings registry as pure metadata**: each setting is declared as data (key, category, kind, label, description, keywords), and that data drives both search and the UI. `crates/codegen/xai-grok-pager/src/settings/registry.rs`
- [ ] `GB-67` **Scoped skills and prompts**: bundled, user, project, and plugin scopes, with a user-invocable flag. `crates/codegen/xai-grok-pager/src/slash/mod.rs`
- [ ] `GB-68` **Fuzzy file search**: an ignore-aware walker feeds a nucleo matcher on a background thread, and it falls back to browse-only if needed. `crates/codegen/xai-fuzzy-file-search/src/lib.rs`

### Config
- [ ] `GB-69` **Layered config with deep merge**: CLI → env → managed requirements → env overlay → `config.toml` → `managed_config.toml` → defaults. `crates/codegen/xai-grok-config/src/lib.rs`
- [ ] `GB-70` **Env overlay**: one env var points to a JSON or TOML overlay that is deep-merged on top, restricted to allowlisted keys. `crates/codegen/xai-grok-config/src/env_overlay.rs`
- [ ] `GB-71` **Version overrides**: `[[version_overrides]]` blocks in each layer are applied based on the binary version. `crates/codegen/xai-grok-config/src/version_overrides.rs`
- [ ] `GB-72` **`$VAR` / `~` expansion** in config string values. `crates/codegen/xai-grok-config/src/loader.rs`
- [ ] `GB-73` **`Lenient<T>` fields**: each field is parsed tolerantly, so one bad or unknown value doesn't reject the whole table. `crates/codegen/xai-grok-status-line/src/config.rs`
- [ ] `GB-74` **Signed managed policy**: an Ed25519-signed, identity-bound policy envelope, checked against compiled-in keys. `crates/codegen/xai-grok-config/src/signed_policy.rs`
- [ ] `GB-75` **Single home override + typed paths**: one env var moves all state (`$GROK_HOME`), and paths use UTF-8 absolute/relative newtypes built on camino. `crates/codegen/xai-dirs/src/lib.rs`, `crates/codegen/xai-grok-paths/`

### Extensibility
- [ ] `GB-76` **Plugin marketplace**: federated git-based sources, canonical GitHub URL normalization, and the official source registered automatically. `crates/codegen/xai-grok-plugin-marketplace/src/lib.rs`

### Testing
- [ ] `GB-77` **PTY e2e harness + ptyctl**: a scenario runner with a mock inference server, alacritty screen capture, and frame timing. ptyctl is a headless PTY controller with an HTTP API: spawn, send keys (vim notation), and read the screen as text, styled text, or HTML. `crates/codegen/xai-grok-pager-pty-harness/src/lib.rs`, `crates/codegen/ptyctl/src/lib.rs`
- [ ] `GB-78` **Mock inference server + mock OTLP collector**: scripted tool calls per conversation, a request log, failure injection, and SSE, plus a collector that records logs, metrics, and traces for assertions. `crates/codegen/xai-grok-test-support/src/lib.rs`
- [ ] `GB-79` **Hermetic test sandbox + timeout scaling**: isolated paths, a clean child env, optional git init, and process-tree teardown. `GROK_TEST_TIMEOUT_SCALE` lets CI stretch every timeout. `crates/codegen/xai-grok-test-support/src/lib.rs`
