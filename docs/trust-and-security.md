# Trust and the security model

For an agent-driven fleet to run without you at the keyboard, atrium relaxes the
permission posture of the agent panes it launches. This page explains the trust
ladder that controls that posture, how the ceiling holds for every caller, and
the seven guarantees that make handing an agent the power to spawn processes safe.

For the request surface these settings govern, see [the control plane](control-plane.md);
for how spawned workers carry a vault identity, see [credential identity](identity.md).

## The trust ladder

The `--trust <plan|accept|automode|skip>` argument sets the **session policy**:
the mode spawned agents run in **and** the ceiling they are capped at. Ordered
low to high power: `plan < accept < automode < skip`.

- **`--trust plan`**: claude's read-only **plan mode**. Agents analyze and
  propose, but make no edits and run no commands.
- **`--trust accept`** (or bare **`--trust`**): auto-accept edits plus a safe
  allowlist of dev commands (detailed below). This is the safe default. Anything
  outside the allowlist still prompts.
- **`--trust automode`**: claude's **auto mode** (`--permission-mode auto`),
  hands-off edits *and* commands under claude's own guardrails. claude only enters
  auto mode if your plan, model, or org allow it; otherwise it falls back to
  default.
- **`--trust skip`** (alias **`--skip-permissions`**): **full bypass**. Appends
  claude's `--dangerously-skip-permissions` (equivalent to `--permission-mode
  bypassPermissions`), so **every** command runs with no gate at all. This is
  genuinely dangerous: an agent, and every teammate it spawns, can delete files,
  push to git, or hit the network unsupervised on your machine. atrium prints a
  plain-English warning and asks you to **confirm at launch** before it starts.

Default (no flag) leaves permissions entirely to claude.

## Ceiling semantics

By default a `ctl spawn` teammate inherits the session policy. Add
**`--mode plan|accept|automode|skip`** to a spawn to pick a different mode for
that teammate, but **only downward**.

The session policy is a ceiling that holds for every caller, with no exception:
a spawn may match it or *de*-escalate below it, and nothing can elevate past it.
To run agents at a higher posture, set it at launch with `--trust`, where it is
one visible, deliberate choice rather than something a pane can ask for
mid-session.

### Why the root pane is no longer carved out

This policy used to carve out the root pane, on the reading that the root pane is
you and you may direct your own session. Two things broke that:

- `-n N` and `--grid` give *every* pane no parent, so every pane in a
  mass-spawned session counted as the operator and could request `skip`, a full
  bypass, in a session set to `plan`.
- A ceiling that a pane can exceed is not a ceiling. Since the human issues `ctl`
  from inside a pane, there was no way to tell "you" from "an agent running where
  you launched it".

## Flag stripping

atrium keeps its policy the single source of truth by neutralizing raw permission
flags an agent might supply in a spawn argv:

- It **strips** raw claude permission flags (`--dangerously-skip-permissions`,
  `--permission-mode`) from agent-supplied spawn argv.
- It **refuses** flags a teammate may not choose at all: `--mcp-config`,
  `--plugin-dir`, `--settings`. Each reaches code execution outside the
  tool-permission system.

Agents request a mode only through `--mode`. Anything atrium ignores, caps, or
refuses is reported in the spawn reply's `note`, never silent.

## The `accept` allowlist

Under `accept`, agents launch in claude's **auto-accept-edits** mode plus an
allowlist of safe dev commands, so the edit/build/test loop runs hands-off.
Anything outside the allowlist (`curl`, `git push`, `rm` outside the working
dir, a critical path) still surfaces as a **visible approval prompt** in its
pane, which shows as a waiting-on-you `?` in the chrome.

The built-in allowlist covers:

- `atrium ctl` — the coordination layer itself, so a fleet's own `bus` / `board` /
  `ctl` calls never prompt (deliberately `atrium ctl`, **not** `atrium`: starting a
  fresh session from inside a pane is not hands-off)
- `python` / `python3` / `pytest`
- `cargo test` / `build` / `check` / `clippy` / `fmt` / `run` / `clean`
- `go test` / `build` / `vet`
- `node` / `npm test`
- the local, non-destructive `git` loop: `status` / `log` / `diff` / `branch` /
  `show` / `add` / `commit` / `merge` / `worktree` — the commit-and-merge loop a
  worktree fleet runs on, deliberately **not** `git push` (network), `reset --hard`,
  or `clean` (destructive), which still prompt

Read-only shell like `ls` / `cat` / `git status` is already auto-accepted by
acceptEdits.

Extend the allowlist with **`ATRIUM_TRUST_ALLOW="cmd one,cmd two"`**. Each
comma-separated prefix `P` becomes a `Bash(P *)` matcher, for example
`ATRIUM_TRUST_ALLOW="just build,make test"`.

## The folder-trust write

Every `--trust` mode (`plan`, `accept`, `automode`, `skip`) also **pre-accepts
claude's separate folder-trust dialog** for each pane's working directory: the
"Do you trust the files in this folder?" gate, stored per-directory in
`~/.claude.json`, which the permission flags do not cover.

atrium writes only that trust bit, only for the pane's own directory, atomically.
A config it can't parse is left untouched and the pane still launches. That write
is the one place atrium touches another tool's config, deliberate and opt-in.

## The security model

Giving a hosted agent the power to spawn processes is a real capability. The
safety rests on **everything being a visible pane you can kill**, plus hard
limits, each one unit-tested as a pure function:

1. **Opt-in.** No `--allow-ctl`, no channel; `atrium ctl` refuses (`ATRIUM_CTL`
   unset). Nothing changes for a session that didn't ask.
2. **Agents, not arbitrary shell.** `ctl spawn` accepts only commands on an
   **agent allowlist** (default `{claude}`), so a confused agent can't
   `ctl spawn -- rm -rf`. Extend it per-session, set by the human who launches
   atrium, with **`ATRIUM_CTL_ALLOW`** (comma-separated stems, for example for
   another agent CLI). It never weakens a session that didn't set it.
3. **No count cap; a depth guard.** A fleet designed for 32 runs 32; width is
   unlimited. Only *recursion* is bounded, by **`--max-depth`** (default 6, `0`
   removes it): a spawn past the ceiling is refused, so a self-spawning agent
   can't fork-bomb the machine. You always see the live count and can kill any
   subtree.
4. **Subtree-scoped control.** By default an agent may `send` / `status` / `kill`
   / `audit` only panes **in its own subtree**; a lead steers its own team, never
   a sibling's. **You control everything**: a root pane (the one atrium opened, or
   any `ctl` run from outside a pane) is the operator and reaches every pane.
5. **Scoped credential delegation.** `ctl spawn --identity X` may only pass down
   an identity the caller **itself holds**: its own identity, or the session or
   fleet default atrium launched with, so a worker can't mint `wif:prod` its lead
   was never granted. You (the human root) are the trust root and may delegate any
   vault identity. Only the identity **name** is ever handled here; the secret is
   re-resolved per spawn and never stored or logged (the same discipline as
   `--identity`).
6. **Audit.** Every request is recorded: caller, action, a **secret-free** detail
   (`send` logs the text *length*, never the body; identities by name only), and
   outcome, to an in-memory ring, readable live via `ctl audit` (subtree-scoped
   like any read). Opt in to a persistent JSONL mirror with
   **`ATRIUM_CTL_AUDIT=<file>`**; a file it can't open is flashed once, and the log
   keeps running in memory.
7. **Visibility is the safety story.** Nothing a `ctl`-spawned agent does is
   hidden: it's a pane with a status border, in the org chart, killable. That is
   the whole reason to do this in atrium instead of as opaque subagents.

See the [project README](../README.md) for the wider picture.
