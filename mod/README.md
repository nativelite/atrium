# The atrium mod

A Claude Code plugin of function hooks that runs inside a pane's `claude` and
tells the atrium broker what the engine knows for certain: the pane's status
at the moment it changes, its context fill and cost, and the text of each
answer. atrium's chrome otherwise infers status from the transcript file;
with the mod, the yellow border appears the instant a permission prompt does.

It speaks `atrium ctl hello` and `atrium ctl report` with the pane's own token
(`ATRIUM_TOKEN`), so it can only ever speak for the pane it runs in. Outside
atrium it does nothing.

Load it for one session:

```bash
claude --plugin-dir /path/to/atrium/mod
```

or for every session a host starts, with `CLAUDE_CODE_PLUGIN_DIRS` naming the
folder. atrium will inject that variable into the claude panes it spawns once
`atrium mod install` exists (PLAN-mod.md, N3).

Check it: `claude plugin validate mod` and `claude plugin test mod`
(`python dev.py check` runs both when `claude` is on PATH).
