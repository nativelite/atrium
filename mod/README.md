# The atrium mod

A Claude Code plugin of function hooks that runs inside a pane's `claude` and
tells the atrium broker what the engine knows for certain: the pane's status
at the moment it changes, its context fill and cost, and the text of each
answer. atrium's chrome otherwise infers status from the transcript file;
with the mod, the yellow border appears the instant a permission prompt does.

It speaks `atrium ctl hello` and `atrium ctl report` with the pane's own token
(`ATRIUM_TOKEN`), so it can only ever speak for the pane it runs in. Outside
atrium it does nothing.

It also gives the model the control plane as typed tools and tells it who it
is:

- **`atrium_*` tools** (`spawn`, `send`, `status`, `list`, `kill`,
  `board_set|get|list|claim|release`, `bus_pub|feed|resolve|topics`, `ask`,
  `asked`, `who`): each
  call is one `atrium ctl` run with the pane's token, and the reply is what the
  model reads. The shell verbs are the same, so the skills stay true. An ask for
  one of these tools is answered by the mod, never a dialog; a deny from a rule
  stands.
- **The `atrium:role` section** of the system prompt, from `atrium ctl whoami`:
  the pane, its role, parent, depth and trust mode, its item on the board and
  the files that item owns, its worktree, whether it may spawn, and what a
  `[atrium bus #` line is. Re-read at every turn, so a lead's re-brief lands.
- **The file guard**: when the item owns files, an `Edit`, `Write` or
  `NotebookEdit` anywhere else is refused with the owner named, by real path,
  and a toast says so. Off when nothing is owned or the pane is in plan mode.

- **Native delivery**: with the `inbox` cap, atrium never types into the
  pane. A `ctl send` or a bus wake waits in the broker's queue until the mod
  collects it (one `atrium ctl inbox --wait` held open while the session is
  interactive, answered the moment a delivery lands and re-issued; an atrium
  that does not hold it is polled every 1.5 s instead) and submits it through
  the engine's own prompt queue, which starts a turn only when the session is
  idle. No draft race, no dialog race, and a task may span lines.
- **The `/atrium` pane and band**: `/atrium` opens a pane inside Claude Code
  with the session's open decisions (each with a resolve button and a hotkey),
  the board, and the last bus events, refreshed every 3 s while open. A band
  above the prompt counts the open decisions and opens the pane.
- **Captions**: every tool call reports what the pane is doing in a few
  words (`running cargo test`, `editing filter.rs`), free; and,
  when the fleet says `captions: true`, a six-word caption from the
  model's own streamed text, by one small `haiku` call at most every twenty
  seconds (`turn.step`). The overview and the tile border show it; the turn's
  end clears it.
- **The collision radar**: the real path of every `Edit`, `Write` and
  `NotebookEdit` that ran is reported (`touched=`); the broker keys it by the
  pane's worktree, lights `⚡` on every pane editing a file another live pane
  edits, and posts one `collision` decision to the lead. `atrium_who {path}`
  asks who has a file.
- **Asks**: a question queued with `atrium ctl ask` (or `Ctrl+A ?`, or
  `atrium_ask` from another pane) rides the inbox as an `ask` item; the mod
  answers it with `$.model.fork`, one tool-less completion over this
  session's own transcript, so the turn is untouched and nothing leaves the
  pane, and reports the reply (`report ask=<id> reply=…`).
- **Subagents as panes**: `atrium_subagent {description, prompt}` spawns a
  claude pane beside this one, sends it the task, waits for the answer its own
  mod reports (`atrium ctl wait --for answer`), closes the pane unless `keep`,
  and returns the answer. Under the session's `subagents: panes` policy (the
  default; a fleet may say `native` or `deny`) the model's Agent tool is
  refused with a pointer to it and its agent types are hidden, so every
  subagent is a tile the human can watch, send to and kill.

Each piece is a capability the mod offers in `hello` (`status`, `answer`,
`context`, `tools`, `guard`, `inbox`, `ask`); the broker's `accepted` list
switches it on.

## When the mod fails

A failure of the mod is never a failure of the pane. Three rules, in
`hooks/health.ts` and at the top of every hook in `hooks/register.ts`:

1. **Every hook catches what its own code throws**, counts it, and lets the
   engine go on exactly as if the mod were not loaded. (The engine also skips
   a hook that throws; the mod does not lean on that, because a skipped hook
   is silent and the mod's job is to say what broke.)
2. **Every `atrium ctl` exchange has a deadline**, and a watchdog of the mod's
   own one second behind it in case the engine's timeout does not fire.
3. **Only atrium says no.** A tool call is refused on a verdict atrium gave:
   the files an item owns (`whoami`), the session's subagent policy. A hook
   that could not ask atrium, or could not judge, lets the call through.

| deadline | value | why |
| --- | --- | --- |
| `hello` | 5 s | awaited inside `session.start`, which the engine awaits before the first prompt; a healthy reply takes tens of milliseconds |
| `whoami` at start | 3 s | the same wait; with `hello`, 8 s at the very worst, under the hook's own 10 s budget |
| `report`, inbox poll, pane refresh, `whoami` between turns | 5 s | background work nobody waits on; past any healthy reply, short enough that a dead broker is noticed in a few calls |
| an `atrium_*` tool | 10 s (`ask`: its own timeout + 30 s) | the call is the tool, and its failure is the tool's result |
| `inbox --wait` | 100 s | a request atrium holds for up to 60 s by design; it runs outside every hook |
| the last report at `session.end` | 3 s | the process may be leaving; a moment, not a hang |
| retry of a hello that could not be asked | once, 5 s later | outside the start, so nobody waits on it |

**Counting and telling.** Failures are counted per hook and ride the next
`report` as three fields: `fail=<hook>:<n>,…` (cumulative), `fail_class` and
`fail_msg` of the last one. The classes: `api` (the plugin interface is not
what the mod was written for: a missing method, `no implementation for …`),
`timeout`, `ctl` (the call could not be made), `shape` (a reply that is not
what the verb answers), `hook` (anything else a hook threw). The message is
the error's name and text with every quoted span, every path and every long
unbroken run replaced by `…`, cut to 120 characters: a tool's input, a file's
contents and the environment never ride in it. A count is keyed by the
hook's name (`tool.call`, `guard`, `feature.inbox`) when the mod's own code
failed, and by `ctl.<site>` (`ctl.hello`, `ctl.report`) when an exchange with
atrium did; atrium holds only the first family against the mod.

**Switching itself off.** After five errors thrown by its hooks, or five ctl
calls in a row that could not be asked, the mod is off for the session: every
hook passes straight through, the loops stop, one last report says why. The
same when atrium answers `mod set aside: …` or accepts no capability.

**A missing method switches off one feature.** The engine's validator lets a
hooks module spell `$` only as a call, never read a method as a value, so
the mod cannot ask beforehand whether `$.prompt.submit` exists: it finds out
at the call. The first `api` failure of a feature switches that feature off
for the session and is reported as `feature.<name>`: `pane` (`/atrium` and
the band), `inbox` (native delivery), `tools`, `guard`, `paths` (real paths;
without `$.fs.stat` they are compared by their text), `captions`, `ask`.

### The audit

What each hook does, what a failure did in 0.2.0, and what it does now.
"Skipped" is the engine's own rule for a hook that throws: the hook is left
out and the chain continues, with one line in the debug log.

| hook | what it does | a failure in 0.2.0 | now (0.3.0) |
| --- | --- | --- | --- |
| `session.start` | `hello`, `whoami`, registers `/atrium`, the pane timer, the inbox loop, the tools; reports the first status | `hello` and `whoami` could each take 10 s, in front of the first prompt; a throw part-way (say `$.command.register`) skipped the hook and silently left everything after it unset; a `hello` reply without an `accepted` list read as "nothing accepted", silently | 5 s + 3 s; each piece (pane, inbox, tools, first status) stands alone and is counted when it fails; a hello that could not be asked is retried once in the background; a reply of the wrong shape is counted (`ctl.hello`, `shape`) |
| `command.run` `/atrium` | opens the pane | a throw skipped the hook: the command answered nothing | answers a line that says the pane could not be opened; counted |
| `ui.render` `Pane` | draws the board, decisions and feed | a throw skipped the hook; the pane fell to the engine's own node, uncounted. The board and the feed never parsed at all: `ctl board list` and `ctl bus feed` were run without `--json` and print a table for a person | the engine's node, counted; ctl is run with `--json`, so the pane shows the board and the feed |
| `ui.render` `AbovePrompt` | the band that counts open decisions | as above | the engine's own band, counted; nothing when the mod is off |
| `agent.spawn`, `agent.offer` | under the `panes` policy, refuses the Agent tool with a pointer to `atrium_subagent` and hides the agent types | refused under `panes` even when `atrium_subagent` did not exist (the `tools` cap not accepted, or its registration failed): the pane was left with no subagents at all | refuses only when the tool it points to was registered; `deny` is atrium's own policy and stands; with the mod off the engine is left alone |
| `prompt.compose` | adds the `atrium:role` section | a throw skipped the hook (the prompt stood), uncounted | the composed prompt as it was, counted |
| `tool.call` `Edit`, `Write`, `NotebookEdit` | the file guard, then the edit, then the touch report | a guard that threw (`$.session.cwd` failing) skipped the hook and the edit ran, uncounted; a path whose folder did not exist yet was refused as "cannot be placed", the file the item owns included, whenever the item owned another file that did exist; a touch report that failed was an unhandled rejection | a guard that cannot judge lets the edit through and is counted; a path the filesystem cannot place is compared by its text, so atrium's list still decides; an edit outside the item's files is refused as before; the touch is caught |
| `turn.step` | reads the step's text for a caption; passes every chunk through | a caption failure was logged; a throw while reading text left the stream where it stood | the chunks pass whatever happens; a missing `$.model.complete` switches captions off |
| `turn.start` | reports `working`, re-reads `whoami` | a throw skipped the hook; the background `whoami` had 10 s | counted; 5 s |
| `tool.check` | answers an ask for an `atrium_*` tool with allow; reports `waiting-approval` | a throw after the verdict skipped the hook (the verdict stood) | the verdict as the engine gave it, counted; a deny is never touched |
| `tool.call` (every tool) | reports `working` and a caption; runs an `atrium_*` tool as one ctl call | a throw in the reporting half skipped the hook and the engine ran the tool; `atrium_list`, `atrium_status`, `atrium_board_*` and `atrium_bus_feed` returned the table for a person inside an `err` field (no `--json`) | the reporting half is caught and counted and the tool runs; an `atrium_*` tool returns the reply itself, and its own failure is its result, never a refusal of another tool |
| `turn.complete` | reports the status, clears the caption, sends the answer | a throw skipped the hook | counted; the turn's result as it was |
| `session.measure` | reports context fill and cost | a throw skipped the hook | counted |
| `session.end` | reports `ended`, stops the loops, waits for the last report | waited on the reporter with no bound of its own (10 s per report still queued) | 3 s, then lets go |
| the reporter | one `report` in flight at a time | 10 s each; a refusal was logged | 5 s; a report that could not be made is counted and not retried by another report; the failure counts ride the next one that goes |
| the inbox loop | holds `inbox --wait`, submits each delivery through `$.prompt.submit` | a refused poll was retried every 1.5 s for the life of the session; a submit that failed was logged and the delivery lost | ten refusals in a row end the polling, and `mod set aside` ends it at once; a failed submit is counted, and when it is the engine lacking the method the inbox is switched off and atrium, told, types the next delivery. The one delivery whose submit failed is still lost: see below |
| the pane timer | refreshes the view every 3 s open, 30 s closed | 10 s per call, and (see `ui.render`) never a reply it could read | 5 s per call; stops when the pane feature is off |

### What atrium does with it

- **The hello window.** A claude pane launched with the mod is expected to
  say `hello` within 30 s of the moment atrium sees it started (its prompt
  hint on screen, or a status known for its session). One that does not is
  *set aside*: a pane without a mod from then on. Its sends are typed, its
  status comes from atrium's status hooks or by inference, and a late hello
  is granted nothing.
- **Its own reports.** An `api` failure, or five failures of its own hooks,
  set aside the mod of the pane that reported them, and no other.
- **The inbox.** A mod that took `inbox` and stops asking for three minutes
  loses that cap alone; what was queued for it is typed.
- **The breaker.** Two voices trip it (two panes atrium saw miss the window;
  or panes set aside by their own reports that are not each other's parent
  and child): new claude panes launch without the mod, a pane that had not
  started is relaunched once without it, a working pane is left alone and
  flagged, and the trip is remembered against the Claude Code version and
  the mod's version and hash until one changes or `atrium mod retry`.

`atrium ctl status <pane>` shows a pane's mod under `"mod"`; `atrium ctl
metrics` counts `mod_failures`; `atrium mod status` says whether the breaker
tripped, against what, and when.

### Known limits

- A delivery the mod took from its inbox and then could not submit is lost
  (one, the one in hand). atrium learns of the failure from the next report;
  when it was the engine lacking the method, the following ones are typed.
- A failure before `session.start`, or of a module that does not load at
  all, cannot be counted by the mod. atrium sees it as a hello that never
  came.
- The launch refreshes the installed files from the binary, so a hand edit
  of the installed mod does not survive a launch.

## Loading it

Load it for one session:

```bash
claude --plugin-dir /path/to/atrium/mod
```

or for every session a host starts, with `CLAUDE_CODE_PLUGIN_DIRS` naming the
folder. atrium carries these files inside its binary: `atrium mod install`
writes them to `mod/` under the platform config directory (or `--at <dir>`),
and every claude pane atrium then spawns gets `CLAUDE_CODE_PLUGIN_DIRS` naming
that folder and `ATRIUM_BIN` naming the atrium that hosts it. `atrium mod
status` says what is installed; `"mod": false` in `config.json` or in a fleet
turns the injection off. Once installed, a launch keeps the folder current
with the atrium that runs, and says so in one line when it rewrote a file.

Check it: `claude plugin validate mod` and `claude plugin test mod`
(`python dev.py check` runs both when `claude` is on PATH).
