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
  collects it (`atrium ctl inbox`, polled every 1.5 s while the session is
  interactive) and submits it through the engine's own prompt queue, which
  starts a turn only when the session is idle. No draft race, no dialog race,
  and a task may span lines.
- **The `/atrium` pane and band**: `/atrium` opens a pane inside Claude Code
  with the session's open decisions (each with a resolve button and a hotkey),
  the board, and the last bus events, refreshed every 3 s while open. A band
  above the prompt counts the open decisions and opens the pane.
- **Captions**: every tool call reports what the pane is doing in a few
  words (`running cargo test --lib ctl`, `editing filter.rs`), free; and,
  unless the fleet says `captions: false`, a six-word caption from the
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
turns the injection off.

Check it: `claude plugin validate mod` and `claude plugin test mod`
(`python dev.py check` runs both when `claude` is on PATH).
