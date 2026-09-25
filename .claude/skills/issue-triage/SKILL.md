---
name: issue-triage
description: Triage this repo's GitHub Issues into the milestone/`agent`-label states the daemon dispatches from. Use when running the weekly triage, clearing the inbox (open issues with no milestone), placing or prioritising a new issue, planning the next milestone after one closes, or filing an issue yourself — an agent-filed issue lands in the inbox and is never labelled `agent` by its author.
---

# Issue triage

The backlog is this repository's GitHub Issues. The daemon reads it directly: an open issue
carrying the `agent` label (plus whatever `tracker.required_labels` and the dispatch rule in
`src/tracker/github.rs` ask for) is work a real agent will pick up and spend tokens on. Triage
is therefore the decision point between "someone wrote this down" and "an agent works this" —
treat it as the gate it is.

## States

Every open issue is in exactly one state, read off two things: its milestone and the `agent`
label.

| State | On GitHub | Meaning |
|---|---|---|
| **Inbox** | open, no milestone | Nobody has looked at it yet. The default for every new issue. |
| **Backlog** | milestone **"Worth doing, not scheduled"** | Triaged and accepted, with no date. |
| **Planned** | any other open milestone | Committed. The milestone *is* the priority. |
| **Dispatchable** | in the **current** milestone **and** labelled `agent` | The daemon may pick it up. |
| **Rejected** | closed as *not planned*, with a one-line reason comment | |

Keeping Backlog as a real milestone is what makes "no milestone" mean *untriaged* and nothing
else, so `is:open no:milestone` is the complete inbox with no extra label to maintain.

Milestones are titled **`M<n> - <outcome>`**: a sequence number, then a human-readable phrase
that says what is true once it closes ("M1 - The daemon can be trusted with its own backlog").
The `M<n>` is the order milestones are worked in — it is the sequence in the title, not
GitHub's internal milestone number, which only records creation. Backlog has no `M<n>`, because
it is not in the sequence. Read the current
set from GitHub rather than from memory; it changes:

```bash
gh api repos/{owner}/{repo}/milestones --jq '.[] | "\(.number)\t\(.title)\t\(.open_issues) open"'
```

## The `agent` label is a human decision

Adding `agent` is what turns an issue into a spawned `claude -p` process with
`bypassPermissions` against a real worktree. So it is added only in triage, only by the
operator or on their explicit say-so, and only to an issue that is:

- **in the current milestone** — `agent` is how the milestone order reaches the daemon, so it
  sits on the current milestone's issues and nowhere else;
- **well-specified** — the issue states the failure and what done looks like, so an agent does
  not spend its turn budget discovering the question;
- **free of an open design choice** — anything that needs a decision (an ADR, "option 1 or 2")
  goes to the operator first; the implementation issues that follow from it can be `agent`.

When you file an issue yourself (found a bug mid-task, split out follow-up work), leave it in
the Inbox with no milestone and no `agent` label, and say in the body what triggered it.
Placing it is triage's job, not the author's.

## Placing an issue

Apply this rule top-down; the first match wins.

1. **It breaks dogfooding or an invariant** — a wrong scheduling decision, lost work, a
   stranded claim, credentials or rate limits that stop the daemon, anything that would fail a
   row of the invariant table in CLAUDE.md. → the **current** milestone. `agent` if it meets
   the bar above.
2. **It unblocks the current milestone.** → the current milestone.
3. **It fits a future milestone's outcome.** → that milestone.
4. **Otherwise** → Backlog. Backlog items are promoted only when a milestone is being planned,
   never one at a time between plannings — that is what keeps the current milestone finishable.
5. **Duplicate, already fixed, or not worth doing** → close as *not planned* with the reason
   (link the duplicate or the PR that fixed it).

"Current milestone" is the open milestone with the lowest `M<n>`.

## Priority is the milestone

There are no priority labels. Milestones are worked one at a time in `M<n>` order, and
that sequence is the whole priority order. Inside a milestone, issues are peers: the daemon
takes them oldest first, and nothing tries to steer that.

So when order inside a milestone *matters* — one issue must land before another, or a fix is
more urgent than the rest — the milestone is too big: **split it**. The part that goes first keeps
its `M<n>`; the rest moves to a new milestone right after it, and every later milestone's `M<n>`
is shifted up by one so the titles still read as the sequence. A milestone whose
issues could land in any order is the right size.

The milestone description lists its issues with one clause each on why they belong to that
outcome. Large work is split into small stacked PRs, each its own issue, stacked by the value
each adds to the outcome rather than by when it was written. The stack itself carries that
order — each PR is based on the one before — so its issues can share a milestone.

## Weekly triage

Run through these in order. Triage is done when every step's criterion holds.

1. **Inbox to zero.** List it:
   ```bash
   gh issue list --state open --search 'no:milestone' --json number,title,labels,createdAt
   ```
   Read each issue in full (`gh issue view <n> --comments`) and place it with the rule above.
   Done when the inbox list is empty.
2. **Stale dispatch.** For every open `agent` issue, check it has a linked PR or recent
   progress:
   ```bash
   gh issue list --state open --label agent --json number,title,updatedAt
   gh pr list --state open --json number,title,headRefName
   ```
   An `agent` issue open more than 7 days with no PR is either under-specified (tighten it)
   or blocked (remove `agent`, say why). Done when each one has a PR, a fresh update, or a
   comment explaining the hold.
3. **Resolved but open.** An issue whose fix has merged (check the invariant table and recent
   PRs) is closed with a comment naming the PR. Done when no merged fix is left open.
4. **Report.** Summarise what moved: issues placed per milestone, labels added or removed,
   issues closed, and anything that needs the operator's decision.

## Planning the next milestone

When a milestone closes:

1. The milestone with the next `M<n>` becomes current. Check its name still states the outcome.
2. Promote from Backlog whatever serves that outcome; leave the rest. If the result has issues
   that must land in a particular order, split it before going on.
3. Update the description's list.
4. Add `agent` to its issues that meet the bar — this is the moment they become dispatchable.

## Acting versus proposing

Moving issues, labelling and closing are writes to a shared tracker under the operator's
credential. Present the full set of proposed changes as one table — issue, from-state,
to-state, labels, reason — and apply it only after the operator confirms. When confirmed,
apply with `gh issue edit <n> --milestone "<title>" --add-label agent` and `gh issue close
<n> --reason "not planned" --comment "<why>"`, and batch the calls: the GitHub API budget is
shared with the running daemon.
