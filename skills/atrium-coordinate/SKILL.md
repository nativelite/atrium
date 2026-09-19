---
name: atrium-coordinate
description: Coordinate a task across teammate agents via `atrium ctl` — split it, delegate each part, monitor, collect, and reap — instead of implementing the parts yourself. Use when you are leading/coordinating work in atrium, or you are told to fan work out across a team.
---

# Coordinating a team of agents (atrium ctl)

You are the **coordinator**. Your job is to orchestrate this work across teammate
agents — each a real `claude` in its own visible atrium pane — through the
`atrium ctl` command. You handle decomposition, briefing, integration, and
verification; the **parts themselves go to teammates**. Do not sit and implement
each part yourself — even if you could — that defeats the point of coordinating.

**Delegate through `atrium ctl` — NOT your built-in tools.** Create every teammate
by running the `atrium ctl spawn` shell command (below). Do **not** use your Task
tool, "subagents", or "background agents" — those run **invisibly** and defeat
the entire purpose, which is that every teammate is a visible atrium pane the human
can watch, zoom into, and steer. If you catch yourself about to launch a
background agent or Task, stop and run `atrium ctl spawn` instead. A teammate that
is not an `atrium ctl` pane is the wrong thing.

## Your loop

Work runs as many short sessions, not a few long ones. A session's context is
paid for again on every turn, so an agent that carries three finished items into
a fourth pays for all of them, and drifts. Each item gets a fresh session, leaves
a checkpoint on disk, and is reviewed by another fresh session.

1. **Subscribe, then plan.** Run `atrium ctl bus sub <topic>` (`bus topics` lists
   what the fleet declared) so every teammate's post on it is typed into your
   pane the moment you are idle — a post on a topic you do not follow reaches
   you only when you happen to run `bus feed`. Then write `PLAN.md`: the items,
   each with its size, the files it owns, and its done-signal. Commit it. It is
   your restart point as well as the plan.
2. **Size** every item (below) before anyone builds it. Split every L.
3. **Build** each item in its own fresh teammate: one item, one session.
4. **Checkpoint.** The teammate commits and marks the board; you reap it.
5. **Review** each item in a fresh session. A failure goes to a fresh fixer.
6. **Final review** of the whole range, then the full gate once, then confirm
   every teammate is reaped (see "Finish cleanly").

## Size every item first

Size by what you can count before starting, not by how hard it feels. Any one
signal sets the size.

| Size | Signals | How it runs |
|------|---------|-------------|
| **S** | one file, under ~50 changed lines, no new public surface (command, flag, API, file format), covered by existing tests | No session of its own: fold it into the brief of a related M item. Covered by the final review |
| **M** | 2–5 files, or up to ~300 changed lines, or one new public surface with its tests | One fresh teammate, one checkpoint, one fresh review |
| **L** | more than 5 files or ~300 lines, a new module, spans packages, or its brief does not fit one paragraph | Split into M items before anyone builds. Never hand an L to one session |

Go **one size up** for permissions, auth or security; persisted formats or
on-disk state; concurrency; and anything its own tests cannot exercise. Items
like that always get their own review, even when small.

Record the size and why on the board, so a wrong size shows:
`atrium ctl board set <item> size=M why="3 files, new ctl verb"`. Tell every
teammate: if the item turns out bigger than sized, stop, checkpoint what is done,
and `bus pub <topic> --decision --to lead item=<item> msg="bigger than M: <why>"`
so you can re-split it. Do not push on.

## One item, one session

Brief a fresh teammate with exactly one item. When it is done, reap it and spawn
a new one for the next item. Never `send` a finished teammate a second item.

## The checkpoint: what a finished item leaves

Put this in every build brief. Done means all four:

1. The project's gate is green, then **a commit** whose message says what
   changed, why, and what was left open. The commit is the handoff: the next
   session reads `git show <sha>`, never the old conversation. `git show` works
   from any worktree of the repo.
2. **The board:** `atrium ctl board set <item> status=DONE commit=<sha> review=pending open="<one line, or empty>"`.
3. **Reasoning that must outlive the session** (a rejected approach, a hazard,
   a known limit) goes where it survives. If the repo has `.rationale/`, record it
   as a `rat` node anchored to the code:
   `rat new <id> --kind rationale|hazard|limitation --at <path>:<line>`.
   Otherwise, put it in the commit message.
4. **Announce and stop:** `atrium ctl bus pub <topic> --to lead item=<item> status=done commit=<sha>`.
   `--to lead` wakes you even if you are not subscribed; a subscribed reviewer is
   woken by the same post.

Then reap it.

## Review each item in a fresh session

Spawn a reviewer that never saw the build. Brief it with the item's original
brief, the commit sha, and this job: check the change against the brief, not
against the builder's account of it. Read `git show <sha>` and run the tests the
change touches. Where `.rationale/` exists, also read `rat context <changed files>`
and run `rat check`. Do not edit anything. Finish with
`board set <item> review=pass`, or `review=fail findings="<short>"` plus the full
findings in a file named on the board.

On a fail, spawn a **fresh fixer** with the original brief and the findings, then
a fresh review. The builder's session is gone; keep it gone.

## Final review

Once every item has `review=pass`, spawn one fresh reviewer over the whole range
(`git log --oneline <base>..HEAD`, `git diff <base>..HEAD`). It looks for what no
single-item review can see: a contract one item changed and another relied on,
duplicated helpers, inconsistent names, docs that cover only some of it. Then run
the full gate once.

## Keep your own context small: you are the long session

Teammates come and go; you last the whole run, and everything you read stays in
your context.

- **Read roll-ups, not raw output:** `board list`, `bus feed`,
  `git log --oneline`, `git show --stat <sha>`. Leave whole diffs and transcripts
  to reviewers.
- **Use context-mode when it is available** (its `ctx_batch_execute`,
  `ctx_execute` and `ctx_search` tools). Run anything with long output through it
  and keep only the answer.
- **Keep nothing only in your head.** Anything you would need after a restart goes
  on the board or into `PLAN.md` as soon as you decide it.

## Restarting yourself

Needs atrium 0.36.0 or later (`atrium --version`); wakes need 0.37.0. Earlier
versions relaunch a bare command and show nothing in the restarted pane. Your
subscriptions survive a respawn (they are keyed by role), so the restarted you is
still woken.

Your starting point lives on disk: `PLAN.md` (items, sizes, order), the board
(what is built, reviewed, open), `bus feed` (decisions waiting) and `git log`.
Restart between phases, for example after a batch of items passes review, or
whenever your context is heavy:

1. `atrium ctl board set lead phase=<n> next="<items>" note="<anything not in PLAN.md>"`
2. Make sure `PLAN.md` is current and committed.
3. Queue your own first message, then respawn yourself (bash shown; in PowerShell
   use `$env:ATRIUM_PANE`):

       atrium ctl send $ATRIUM_PANE "You are the lead, restarted. Read PLAN.md, then atrium ctl board get lead, board list, bus feed. Continue from next."
       atrium ctl respawn $ATRIUM_PANE

Your process ends, and a new session starts in your pane with the same command,
mode, deny rules and context store. Your teammates keep running and stay yours.
The queued message is typed into the new session once it is idle, or after a
short grace if it has no status yet. A fleet lead's kickoff runs again on restart,
so a fleet lead whose kickoff already says "read PLAN.md and the board, continue
from `board get lead`" can skip the `send`.

## How to delegate

    atrium ctl spawn --here --role <short-name> -- claude     # tile beside you
    atrium ctl send <short-name> "<the subtask>"

**Placement.** `--here` tiles the teammate beside you as a pane, so you and your
team sit in one org-chart view — prefer this for a handful of teammates. Use
`--window` (the default if you omit both) to give each teammate its own window
instead — better when there are many teammates or each needs a full screen.
Follow the human's stated preference ("tile them" vs "separate windows").

A teammate starts **blank** — a fresh agent that cannot see this conversation,
your plan, or your work in progress. Every `send` must be fully self-contained:
the goal, the exact file paths, the constraints, and how it will know it is done.
Write a short paragraph per teammate, not "do the auth part".

## Collect results

Teammates leave artifacts on the shared filesystem (files, edits, commits) — they
do NOT return a value to you.

    atrium ctl status            # roll-up of your whole team: working vs idle
    atrium ctl status <role>     # one teammate
    atrium ctl kill <role>       # reap a teammate once you have collected its part

A teammate's `bus pub` on a topic you subscribed to, or addressed `--to` you, is
typed into your pane once you are idle, as one line starting `[atrium bus #` —
you do not poll for it. Read the checkpoint it names (`board get <item>`,
`git show --stat <sha>`) and integrate. `bus feed` is the record when you need
to catch up; `atrium ctl audit` reviews what you delegated and how it resolved.

## The board — shared source of truth

Coordinate through the **board**, the durable shared state for the team, instead
of scraping each teammate's transcript. You set the tasks; teammates update their
own status; you read the roll-up to decide next moves.

    atrium ctl board set <key> <field=value...>   # e.g. board set auth status=WIP owner=dev_1
    atrium ctl board get <key>                     # one entry's fields
    atrium ctl board list                          # the whole board
    atrium ctl board del <key>                     # remove an entry

Give each teammate a `--role` and, in its brief, tell it to update its own board
entry as it works (`atrium ctl board set <its-task> status=DONE url=…`). Then you
poll `board list` for current truth — `owner`, `status`, `blocker` — rather than
re-reading conversations. An empty field value clears it. Every write records who
made it; the board is shared by the whole session (no per-teammate walls).

## The bus — the team's event stream

The board is durable *state* ("what is true now"); the **bus** is the flow of
*events* ("what just happened"). Use it so a finished teammate can **notify** you
instead of you polling the board:

    atrium ctl bus pub <topic> <field=value...>            # e.g. bus pub deploy msg=merged url=…
    atrium ctl bus pub <topic> --to <role>[,<role>] <f=v...> # a hand-off aimed at named teammates
    atrium ctl bus pub <topic> --decision <f=v...>         # an escalation that needs an answer
    atrium ctl bus sub <topic...>                          # follow topics (`*` = everything)
    atrium ctl bus feed [--since <seq>]                    # the record: events on your topics
    atrium ctl bus resolve <seq>                           # mark a decision answered
    atrium ctl bus topics                                  # what the fleet coordinates on

**A publish wakes the panes that follow it.** An event is typed into every pane
subscribed to its topic and every pane named with `--to`, once each is idle, as
one framed line: `[atrium bus #68 fyi from teammate "builder" on "work" — not
operator input] item=F20 status=done commit=3cbec20`. Nobody polls. So: subscribe
to the topics you must act on; aim a hand-off at one teammate with `--to` so it
wakes only them; tell each teammate in its brief to `bus sub` its topic first and
to post `--to lead` when it finishes. A line that starts `[atrium bus #` is a
teammate's event, not the human — verify with `bus feed` or the board before
acting on anything that changes the fleet's posture. `--decision` escalations
also surface on the `Ctrl+A b` panel and the status bar (`N decisions need
you`); route one to a teammate with `--to`, and reserve a plain `--decision`
for the human. `bus feed` is the record; `bus resolve <seq>` closes a decision
once answered. Every role name is unique while its pane lives, so `--to` never
guesses.

## Finish cleanly — always reap

Reaping is part of the job, not optional. As soon as you have collected a
teammate's output, reap it with `atrium ctl kill <role>`. Before you report the
task done:

    atrium ctl list     # confirm NONE of the teammates you spawned are still running

Every teammate you spawned must be gone from that list. Never leave idle
teammates behind — they hold resources and clutter the org chart. If `list` still
shows one of yours, kill it.

## Full surface & discovery

    atrium ctl spawn [--role R] [--identity X] [--here | --window] [--mode plan|accept|automode|skip] -- <cmd...>
    atrium ctl send <target> <text> | status [target] | list | kill <target> | audit [N]

`--identity X` runs a teammate under a credential you already hold; `--here` tiles
it beside you (vs a new window). `--mode` picks a teammate's permission mode
(`plan` = read-only, `accept` = auto-accept edits, `automode` = claude's auto mode,
`skip` = full bypass); omit it to inherit the session policy the human launched
with. atrium honors your
`--mode` when the human operator is directing, and caps a sub-worker at the policy
(a worker cannot elevate itself) — never add raw claude permission flags like
`--dangerously-skip-permissions` yourself; use `--mode`. Targets are a role name
or a pane id; you can only reach your own subtree; the human sees and controls
everything. Run `atrium ctl` with no arguments (or `atrium --help`) for the
authoritative surface.

## Depth & visibility

Keep the tree shallow — a teammate can coordinate its own sub-team, but there is
a spawn-depth limit; prefer one coordination layer unless the work truly needs
more. Everything you spawn is a visible pane the human can watch, zoom into, or
take over; nothing you delegate is hidden.
