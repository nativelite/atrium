# Agent-aware status

When atrium launches an agent pane, it binds that pane to the agent's own session and lets the agent's real status tint the pane's chrome. In a grid of agents, this lets you see at a glance which one is stuck waiting on you. This page covers the three status signals atrium renders, how the session bind works, and exactly what atrium can and cannot see.

## The three status signals

atrium surfaces an agent's state in three places, so a blocked agent shows up whether its pane is focused, tiled in the background, or in a window you are not currently viewing.

### Border color (unfocused, tiled panes)

| Color | Meaning |
| --- | --- |
| Bright yellow | The agent is waiting on your approval. |
| Grey | Waiting for a prompt, or idle. |
| Default | Working. |

The focused pane keeps its normal bright-cyan border. A blocked agent in the pane you are already looking at needs no nagging, so it is left alone.

### Title badge

The pane's top-edge label gains a single marker:

- ` 2:claude ? ` when the agent is waiting on you.
- ` 2:claude ~ ` while the agent is working.
- Nothing when the agent is idle.

### Status bar

A window whose bound agent is waiting shows `?` next to its name, and the bar appends `| N waiting`. This means a blocked agent in a backgrounded window still surfaces, even when it is off-screen.

### Terminal title and the "needs you" ring

The three signals above need you to be looking at atrium. When it sits in a background tab or window, the host terminal carries the state instead. atrium sets the terminal's title to `atrium · <name> · <state>`, where the name is the fleet's name (or its `title` key), else the project directory, and the state is, in precedence order, `? builder needs you · 2 working` / `? 2 need you` / `? 1 decision needs you`, then `● 3 working`, then `○ idle`. Idle, waiting-for-a-prompt and unbound panes count as idle; an exited pane is neither. The title is written only when it changes, and the host's own title is saved at startup and restored at exit.

A *transition* into "needs you" rings: a pane newly waiting on your approval (once per pane, with a 30 s cooldown) or a decision newly addressed to you on the bus. Channels: `BEL`; an `OSC 9` toast (iTerm2, ConEmu, fwterm); and, for Windows Terminal, which ignores a plain `OSC 9`, the tab progress indicator, red while anything needs you and cleared when nothing does. The `notify` key in `config.json`, and per fleet, is `both` (the default), `bell`, `toast` or `off`.

Known limits: without the mod or the settings hooks, a permission prompt rings about eight seconds after it appears (the transcript inference's dwell plus the refresh); working to idle lags by a minute. Windows Terminal shows no toast, so the bell and the tab indicator are its signals; ConPTY has no title stack, so the empty-title reset restores the tab's own name. Under tmux, `OSC 2` sets the pane title (the outer title with `set-titles on`), `OSC 9` needs `allow-passthrough`, and the bell follows `bell-action`. A pane's own title escapes (`OSC 0/1/2`, including ConPTY re-emitting a child's title) are stripped in passthrough so a child never fights atrium for the tab.

## How the bind works

When atrium spawns an agent pane, it mints a fresh session id and passes `claude --session-id <uuid>`. If you already supplied a session-selecting flag such as `--resume`, `--continue`, or `--session-id`, the id is yours and atrium leaves it alone.

Claude Code writes its transcript as `<uuid>.jsonl`, so atrium knows the exact file to watch. This is a direct lookup with no guessing, even when several agents share one directory.

The status itself is derived by the [`agsess`](https://github.com/nativelite/agsess) crate, which reads those transcript files read-only.

## What atrium can and cannot see

atrium reads the status of Claude Code sessions it launched, and nothing more.

### What it reads

- Only the session transcript files (`*.jsonl`) that Claude Code writes under your own `~/.claude/projects`, on your own disk.
- Only the transcripts for sessions atrium started via `--session-id`.

It is read-only and status only: the border and bar render whether an agent is working, waiting, or idle, and never any conversation text. No prompt and no reply ever leaks from one pane into another pane's chrome. No credential file is ever opened, and there is no network path at all.

### What it does not see

- An agent you started inside a shell pane (`$ claude`). That pane's title is your shell, so atrium never binds it. This is a documented v1 limitation.
- Agents running in other tools.
- Sessions you run outside atrium entirely. That is what [`agtop`](https://github.com/nativelite/agtop) is for.
- Anything that would require credentials.

Because a pane atrium did not launch as an agent is never bound, non-agent panes (shells, `vim`, `cmd`) never acquire agent chrome and never cost a scan.

## The waiting inference and `permissionMode`

The waiting-on-you status is an inference over the transcript tail, gated by Claude Code's `permissionMode`. On a machine running mostly in `auto` mode, the loud yellow border correctly stays quiet: auto mode does not block on the human, so there is nothing to surface. The yellow is reserved for the sessions where an agent is genuinely blocked.

The mod replaces the inference with a certainty, below.

## Certainty: the mod

`atrium mod install` writes a small Claude Code plugin out of atrium's binary, and every claude pane atrium then spawns loads it (`CLAUDE_CODE_PLUGIN_DIRS`). Inside the engine the mod hooks the turn and tool events and reports the pane's status to atrium the moment it changes, over the same control channel `atrium ctl` uses and with the pane's own capability token, so a pane can only ever speak for itself.

| reported | when |
| --- | --- |
| `working` | a turn started, or a tool call ran |
| `waiting-approval` | a permission check resolved to an ask, with the tool as the reason |
| `waiting-prompt` | the turn ended with an answer, or was interrupted |
| `idle` | nothing has happened for a while |
| `errored` | the turn ended on an API error or a refusal: the border turns **red** with a `!` badge, the bar counts it with the waiting panes, and the overview marks it ‼ |
| `ended` | the session ended |

With the mod a pane also has a **caption**: what it is doing right now, in a few words, from the tool it is running (`running cargo test`, `editing filter.rs`) and, when a fleet says `captions: true`, from its own words (a six-word summary by a small model, at most every twenty seconds of streaming). The overview shows the caption in place of the transcript's last action and an unfocused tile draws it dim after the title when the tile is wide enough; the turn's end clears it. A `⚡` after the badge means the pane edited a file another live pane edited too (the collision radar, [control plane](control-plane.md#the-mod-typed-tools-the-role-section-and-subagent-panes)). And `Ctrl+A ?` on a tile asks the pane what it is doing, what it needs and what blocks it: its mod answers from a fork of its own context without interrupting it, and the reply opens as an overlay (any key closes it; `atrium ctl asked <pane> <id>` reads it again). A pane without the mod is told so in the bar; nothing is typed into it.

A reported status wins over the inferred one for two minutes, then the inference is used again, so a mod that died cannot pin a pane. The mod also reports the context-window fill and the cost so far, which the overview (`Ctrl+A o`) shows per agent (`ctx 41% $0.12`) and summed per window, and the text of each answer, which only the pane that spawned it as a subagent may read (`atrium ctl answer`, `wait`). It never sends conversation text anywhere else. Codex and gemini panes, and a claude without the mod, keep the inference. `atrium ctl status` and `ctl list` show the reported label, including `errored` and `ended`.

## Certainty without the mod: settings hooks

A claude pane in a ctl session (`--allow-ctl`, or any fleet) gets certainty status even without the mod. Claude Code runs *settings hooks*, shell commands on named events, and takes a `--settings <file>` flag that adds a settings file for one session on top of the user's own. atrium writes one such file beside its state (`hooks/claude-status.json`, next to the sessions store) with a command per event that runs `atrium ctl report status=… --quiet` with the pane's own token, inherited from the pane's environment, and passes it to every bound claude pane whose command line does not already carry `--settings` (a user's flag always wins). `UserPromptSubmit`, `PreToolUse` and `PermissionDenied` report `working`; `PermissionRequest` reports `waiting-approval`; `Stop` reports `waiting-prompt`; `SessionEnd` reports `ended`. Each hook prints nothing and exits 0, which for all of these events means "no decision": the permission prompt still shows, the tool still runs. The broker treats these reports as it treats the mod's, ahead of the transcript inference, so the yellow border lands within one tick of the prompt instead of seven to eight seconds later. The flag is added only after `claude --help` has shown that this claude takes `--settings`.

## See also

- [atrium README](../README.md)
- [Agent identity](identity.md)
- [The control plane](control-plane.md)
