---
name: supervise-crewd
description: Watch a running crewd from the operator's session with a read-only /loop over the crew_ops tools.
disable-model-invocation: true
---

# Supervise crewd

Start a `/loop` that watches the daemon. Invoke the `loop` skill with the arguments below. Use the
interval the operator passed to this command, or `15m` if they passed none. An interval of `auto`
means passing no interval, so the loop sets its own pace.

```
15m Check crewd with the crew_ops snapshot tool and report only what changed since the last check: runs started or finished, a pull request opened or updated, a quarantine, a rate-limit pause, a delivery handoff, a new last_error. When a pull request's delivery stage becomes ready, run /code-review on it and summarise. Read only: never call refresh, unquarantine or unblock, and never push to a branch the daemon owns, without asking.
```

Before starting, check that the `crew_ops` tools are connected. If they aren't, the daemon was
started without `--mcp` or `api.mcp_enabled`, or the server isn't registered in this session's
local scope. Say which, and stop.

## Why it is read-only

`refresh`, `unquarantine` and `unblock` change what the scheduler does next. A push from outside the daemon
onto a branch it is delivering is exactly what delivery's `--force-with-lease` exists to refuse,
and the push becomes a permanent handoff. Report, and let the operator decide.

## Why 15 minutes

A session is a few minutes of turns, and a pull request's CI and review take longer. So each
check sees a transition without spending one on nothing.
