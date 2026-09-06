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

1. **Split** the work into independent parts — ideally parts that can proceed in
   parallel without waiting on each other.
2. **Delegate each part** to its own teammate (see below). Delegate the part
   rather than doing it yourself.
3. **Monitor** with `atrium ctl status`; when a teammate reports idle, collect its
   output (the files it changed).
4. **Integrate and verify** the parts, resolve conflicts, and **reap every
   teammate you spawned** (see "Finish cleanly"). Synthesis and cleanup are your
   job.

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

Poll status until teammates are idle, then read the files they changed and
integrate. Use `atrium ctl audit` to review what you delegated and how it resolved.

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

    atrium ctl bus pub <topic> <field=value...>    # e.g. bus pub deploy msg=merged url=…
    atrium ctl bus pub <topic> --decision <f=v...> # an escalation that needs YOUR answer
    atrium ctl bus sub <topic...>                  # follow topics (`*` = everything)
    atrium ctl bus feed [--since <seq>]            # pull new events on your topics
    atrium ctl bus resolve <seq>                   # mark a decision answered

Subscribe to the topics you own (or `*` as the lead), and tell each teammate in
its brief to `bus pub` an update when it finishes a unit of work and to use
`--decision` when it's blocked on a call only you can make — those surface on the
`Ctrl+A b` panel and the status bar (`N decisions need you`). Default `fyi` events
are cheap; you only pull the topics you subscribed to, so keep chatter on-topic.
Poll `bus feed` between steps to see what landed, and `bus resolve <seq>` each
decision once you've answered it.

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
