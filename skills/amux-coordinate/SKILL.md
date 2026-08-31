---
name: amux-coordinate
description: Coordinate a task across teammate agents via `amux ctl` — split it, delegate each part, monitor, collect, and reap — instead of implementing the parts yourself. Use when you are leading/coordinating work in amux, or you are told to fan work out across a team.
---

# Coordinating a team of agents (amux ctl)

You are the **coordinator**. Your job is to orchestrate this work across teammate
agents — each a real `claude` in its own visible amux pane — through the
`amux ctl` command. You handle decomposition, briefing, integration, and
verification; the **parts themselves go to teammates**. Do not sit and implement
each part yourself — even if you could — that defeats the point of coordinating.

**Delegate through `amux ctl` — NOT your built-in tools.** Create every teammate
by running the `amux ctl spawn` shell command (below). Do **not** use your Task
tool, "subagents", or "background agents" — those run **invisibly** and defeat
the entire purpose, which is that every teammate is a visible amux pane the human
can watch, zoom into, and steer. If you catch yourself about to launch a
background agent or Task, stop and run `amux ctl spawn` instead. A teammate that
is not an `amux ctl` pane is the wrong thing.

## Your loop

1. **Split** the work into independent parts — ideally parts that can proceed in
   parallel without waiting on each other.
2. **Delegate each part** to its own teammate (see below). Delegate the part
   rather than doing it yourself.
3. **Monitor** with `amux ctl status`; when a teammate reports idle, collect its
   output (the files it changed).
4. **Integrate and verify** the parts, resolve conflicts, and **reap every
   teammate you spawned** (see "Finish cleanly"). Synthesis and cleanup are your
   job.

## How to delegate

    amux ctl spawn --here --role <short-name> -- claude     # tile beside you
    amux ctl send <short-name> "<the subtask>"

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

    amux ctl status            # roll-up of your whole team: working vs idle
    amux ctl status <role>     # one teammate
    amux ctl kill <role>       # reap a teammate once you have collected its part

Poll status until teammates are idle, then read the files they changed and
integrate. Use `amux ctl audit` to review what you delegated and how it resolved.

## Finish cleanly — always reap

Reaping is part of the job, not optional. As soon as you have collected a
teammate's output, reap it with `amux ctl kill <role>`. Before you report the
task done:

    amux ctl list     # confirm NONE of the teammates you spawned are still running

Every teammate you spawned must be gone from that list. Never leave idle
teammates behind — they hold resources and clutter the org chart. If `list` still
shows one of yours, kill it.

## Full surface & discovery

    amux ctl spawn [--role R] [--identity X] [--here | --window] [--mode plan|accept|automode] -- <cmd...>
    amux ctl send <target> <text> | status [target] | list | kill <target> | audit [N]

`--identity X` runs a teammate under a credential you already hold; `--here` tiles
it beside you (vs a new window). `--mode` picks a teammate's permission mode
(`plan` = read-only, `accept` = auto-accept edits, `automode` = full bypass);
omit it to inherit the session policy the human launched with. amux honors your
`--mode` when the human operator is directing, and caps a sub-worker at the policy
(a worker cannot elevate itself) — never add raw claude permission flags like
`--dangerously-skip-permissions` yourself; use `--mode`. Targets are a role name
or a pane id; you can only reach your own subtree; the human sees and controls
everything. Run `amux ctl` with no arguments (or `amux --help`) for the
authoritative surface.

## Depth & visibility

Keep the tree shallow — a teammate can coordinate its own sub-team, but there is
a spawn-depth limit; prefer one coordination layer unless the work truly needs
more. Everything you spawn is a visible pane the human can watch, zoom into, or
take over; nothing you delegate is hidden.
