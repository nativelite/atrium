---
name: atrium-delegate
description: Delegate an independent, substantial part of your work to a teammate agent (a claude in its own atrium pane) via `atrium ctl`, when it genuinely pays off. Use when running inside atrium and part of the task is large and independent.
---

# Delegating to a teammate agent (atrium ctl)

You are running inside atrium, which lets you spawn teammate agents — each a real
`claude` in its own visible pane — through the `atrium ctl` command. You MAY hand
an independent part of your work to a teammate.

**Delegate through `atrium ctl` — NOT your built-in tools.** If you delegate, do it
by running the `atrium ctl spawn` shell command (below), never with your Task tool,
"subagents", or "background agents" — those run invisibly, whereas an `atrium ctl`
teammate is a visible pane the human can watch and steer. That visibility is the
whole point.

## Delegate only when it pays

Delegate a part when it is **genuinely independent** and **substantial enough**
that running it in parallel beats doing it yourself. If you could finish the work
about as fast as it takes to spawn, brief, monitor, and reap a teammate, just do
it — delegation has real coordination overhead. Do NOT delegate parts that depend
on each other, or small/quick work. (If you have been told to coordinate a team,
use the atrium-coordinate skill instead — there, delegating is the job.)

Size the part by what you can count. One file and under ~50 changed lines, with no
new command, flag, API or file format, is small: do it yourself. Two to five
files, up to ~300 lines, or one new public surface with its tests is worth one
teammate. Anything bigger is several parts; split it before handing any of it
off. Give each teammate **one** part, and never send a finished teammate another.
A fresh teammate per part keeps every session small.

## How to delegate

    atrium ctl spawn --role <short-name> -- claude
    atrium ctl send <short-name> "<the subtask>"

A teammate starts **blank** — a fresh agent that cannot see this conversation,
your goal, or your work in progress. Every `send` must be fully self-contained:
the goal, the exact file paths, the constraints, and how it will know it is done.
Write a short paragraph, not "do the auth part".

End every brief with the checkpoint, so the part's result survives its session:
run the project's gate, then commit with a message saying what changed, why, and
what is left open. Record reasoning that must outlive the session as a `rat` node
if the repo has `.rationale/`, or in the commit message otherwise. Finish with
`atrium ctl board set <part> status=DONE commit=<sha>`.

## Collect results, then reap

Teammates leave artifacts on the shared filesystem (files, edits, commits) — they
do NOT return a value to you.

    atrium ctl status [<role>]   # which teammates are working vs idle
    atrium ctl kill <role>       # reap a teammate once you have collected its part

Tell each teammate in its brief to post `atrium ctl bus pub <topic> --to <your
role> msg=done commit=<sha>` when it finishes: that line is typed into your pane
once you are idle (`[atrium bus #… from teammate "<role>" …]`), so you need not
poll. Then read its checkpoint (`board get <part>`, `git show <sha>`) and check
the work against the brief, then reap it. Don't leave a finished teammate
running.

## Full surface & discovery

    atrium ctl spawn [--role R] [--identity X] [--here | --window] [--mode plan|accept|automode|skip] -- <cmd...>
    atrium ctl send <target> <text> | status [target] | list | kill <target> | audit [N]
    atrium ctl bus pub <topic> [--to <role>[,...]] <field=value...> | bus sub <topic...>

`--identity X` runs a teammate under a credential you already hold; `--here` tiles
it beside you (vs a new window); `--mode` picks a teammate's permission mode
(`plan`/`accept`/`automode`/`skip`), else it inherits the session policy — atrium caps you
at that policy unless the human operator is directing, so use `--mode` rather than
raw claude permission flags; `audit` prints your delegation ledger. Targets are a
role name or a pane id; you can only reach your own subtree. Run `atrium ctl` with
no arguments (or `atrium --help`) for the authoritative, current surface.

## Depth & visibility

Keep the tree shallow — a teammate can delegate too, but there is a spawn-depth
limit. Everything you spawn is a visible pane the human can watch, zoom into, or
take over; nothing you delegate is hidden.
