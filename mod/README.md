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
folder. atrium carries these files inside its binary: `atrium mod install`
writes them to `mod/` under the platform config directory (or `--at <dir>`),
and every claude pane atrium then spawns gets `CLAUDE_CODE_PLUGIN_DIRS` naming
that folder and `ATRIUM_BIN` naming the atrium that hosts it. `atrium mod
status` says what is installed; `"mod": false` in `config.json` or in a fleet
turns the injection off.

Check it: `claude plugin validate mod` and `claude plugin test mod`
(`python dev.py check` runs both when `claude` is on PATH).
