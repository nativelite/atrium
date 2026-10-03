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
  `board_set|get|list|claim|release`, `bus_pub|feed|resolve|topics`): each
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

- **Subagents as panes**: `atrium_subagent {description, prompt}` spawns a
  claude pane beside this one, sends it the task, waits for the answer its own
  mod reports (`atrium ctl wait --for answer`), closes the pane unless `keep`,
  and returns the answer. Under the session's `subagents: panes` policy (the
  default; a fleet may say `native` or `deny`) the model's Agent tool is
  refused with a pointer to it and its agent types are hidden, so every
  subagent is a tile the human can watch, send to and kill.

Each piece is a capability the mod offers in `hello` (`status`, `answer`,
`context`, `tools`, `guard`); the broker's `accepted` list switches it on.

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
