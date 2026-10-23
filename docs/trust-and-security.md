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

## codex panes

codex has no `--permission-mode`. atrium maps each posture to codex's own
approval and sandbox settings instead:

| Posture | codex gets |
| --- | --- |
| `plan` | no approvals asked, read-only sandbox |
| `accept`, `automode` | no approvals asked, sandbox confined to the workspace |
| `skip` | `--dangerously-bypass-approvals-and-sandbox` (no sandbox at all) |

codex's coarser model has no per-command allowlist, so `accept` and `automode`
are the same for it, and `deny` rules (claude's `--disallowedTools`) don't bind
a codex pane.

**The control socket is let through the sandbox, and nothing else.** On
macOS and Linux, codex's sandbox counts a connection to a unix socket as
network, and both of its sandboxes have no network, so a codex pane at `plan`,
`accept` or `automode` could not run `atrium ctl` at all (`Operation not
permitted`). With the control plane on, atrium now launches such a pane under a
codex permission profile,
`atrium_ctl`, in place of `--sandbox`. It is codex's own `:workspace` (or
`:read-only` for `plan`) with one change: the network is on, but routed through
codex's proxy in `limited` mode with **no domain allowed** and **one** unix
socket allowed, this session's `ATRIUM_CTL`. Measured with codex 0.163.0 on
macOS, under that profile:

- `atrium ctl whoami` and `atrium ctl status` work from the codex pane;
- any other unix socket, HTTPS and raw TCP are refused;
- writes stay inside the workspace, exactly as before.

This is on by default, because a coordinating codex pane is useless without
it. What it costs: the pane's commands can talk to this atrium session's
control plane, with that pane's capability token, the same access a claude
pane has. The spawn log (`ATRIUM_SPAWN_LOG`) shows the exact arguments.

**git works in the sandbox.** codex's `:workspace` keeps a checkout's `.git`
read-only, and for a linked worktree the git dir its `.git` file points at
too, so `git add` and `git commit` in an `accept` or `automode` codex pane
failed with `Read-only file system` (measured, codex 0.163.0 on Linux:
`Unable to create '<main>/.git/worktrees/<name>/index.lock'`). A fleet item
is done only with a commit, so a codex pane at those postures whose working
directory is a checkout now gets filesystem rules in the same profile, socket
or not: its git dir and the common dir (`git rev-parse --git-dir
--git-common-dir`) writable, with `hooks/`, `config` and the worktree's
`config.worktree` kept read-only, the files that would run code as you the
next time you use git outside the sandbox. Measured under these rules: a
commit succeeds in a main checkout and in a linked worktree; creating a hook
and appending to `config` are refused. `plan` stays read-only, git included.

**The rest of a codex pane's permissions are the fleet file's to set.** A
`codex` block, on the fleet or on one codex agent, refines what happens
inside the posture `trust` chose, in codex's own terms: its approval policy
(`approval`), whether git stays read-only (`git`), paths kept read-only
inside the workspace (`read_only`), the network through codex's proxy to
every host or the listed ones (`network`, `domains`, `deny_domains`),
loopback and local servers (`local`), further unix sockets
(`unix_sockets`), and `raw`, any other key of codex's permission profile
passed through as written — so the block reaches the whole of that profile,
not a subset atrium chose. atrium folds it into the same `atrium_ctl`
profile and the banner names each distinct block with the agents it reaches. The
posture still wins: `plan` takes only the approval and the network, `skip`
ignores the block, and an agent's `ctl spawn` cannot set one. See
[codex permissions](fleets.md#codex-permissions).

Limits:

- **It needs codex's `network_proxy` feature**, which codex marks
  experimental. atrium asks each codex program once per session
  (`codex features list`); one without it keeps the plain `--sandbox`, and
  the bar says its commands can't reach ctl. Update codex, or run that pane at
  `--mode skip`.
- **Linux is untested.** The profile is the same, but only macOS was measured.
- **codex's own folder-trust dialog is not pre-accepted.** codex asks "Trust
  this folder?" the first time it runs in a directory and saves the answer in
  `~/.codex/config.toml`; atrium does not write that file, so a hands-off codex
  pane in a new directory waits on the dialog until someone answers it.

**On Windows the pipe admits the sandbox itself.** There codex's sandbox is
not a network filter but a *restricted token*: the agent's commands run as the
same user, with an access check that must pass twice, against the token's
groups and again against its restricting SIDs, which are the logon session,
Everyone and one SID of codex's own, not the user. atrium's control pipe
granted the user and SYSTEM only, which fails the second check, so a sandboxed
`atrium ctl` got `ACCESS_DENIED` and the pane could receive but never reply.
The pipe's access list now also names **atrium's own logon SID**, the one SID in
both lists. That widens nothing: a process of this user in this logon session
could open the pipe already, and another user's, or another logon session's,
still can't. No profile and no `network_proxy` are needed on Windows; the pane
keeps its plain `--sandbox`. Measured with codex 0.163.0: `atrium ctl list`
run under `codex sandbox` from inside a live pane gets its reply
(`tests/atrium/ctl.rs`, `ctl_list_roundtrips_from_inside_a_codex_sandbox`,
ignored by default because it needs a `codex` on PATH), and a two-agent fleet,
a claude `lead` and a codex `dev` at `accept`, in which the codex pane's
`atrium ctl whoami` answers with its role (`tests/atrium/fleet.rs`,
`a_claude_lead_and_a_sandboxed_codex_dev_fleet_reaches_ctl`, ignored too: it
needs both programs logged in).

One Windows condition is codex's, not the pipe's, and atrium can only name it:
codex runs the agent's commands through `pwsh` when one is on PATH, and its
restricted token **cannot start the Microsoft Store build of PowerShell** (a
packaged app under `WindowsApps`): `CreateProcessAsUserW` fails with `Access is
denied` before the command runs, so *every* command fails, `atrium ctl`
included, and codex reports that it was denied or could not connect. When a
sandboxed codex pane spawns and `pwsh` resolves into a `WindowsApps` directory,
the bar says so. The fix is outside atrium: install a non-packaged PowerShell 7
(`winget install Microsoft.PowerShell`) ahead of the Store one on PATH, or
remove the Store build; codex without any `pwsh` falls back to Windows
PowerShell, which its sandbox runs fine.

### What a codex command may carry

A codex `cmd` (in a fleet, or after `--` in `ctl spawn`) may carry **any** codex
option — `--enable`/`--disable <feature>`, `-c <key>=<value>`, `--search`,
`-i`, `--oss`, `--local-provider`, … — except the ones that decide what codex
is permitted to do. claude's argv is vetted against a short allowlist; codex's
the other way round, so a fleet can use codex as its author wants.

| Class | Options | What atrium does |
| --- | --- | --- |
| Posture | `-s/--sandbox`, `-a/--ask-for-approval`, `--dangerously-bypass-approvals-and-sandbox` and its hidden alias `--yolo`, `--approve-for-me`, `--dangerously-bypass-hook-trust`, `-p/--profile`, `--remote`, `--remote-auth-token-env`; `-c` keys `sandbox_mode`, `approval_policy`, `approvals_reviewer`, `default_permissions`, `permissions.*`, `sandbox_workspace_write.*`, `profile`, `profiles.*`, `developer_instructions` | **stripped** and named: a preflight warning on `fleet up`, the reply's `note` on `ctl spawn`. Posture comes from `trust` / `--mode` only, and `developer_instructions` is atrium's channel for `prompt`. |
| Placement | `-C/--cd`, `--add-dir` | **refused**: they widen what the sandbox may write (`-C /` makes the disk the workspace). Use the agent's `cwd` and `add_dirs`, which the banner shows. |
| Out of the sandbox | `-c` keys `mcp_servers.*`, `notify`, `hooks.*`, `model_providers.*`, `openai_base_url`, `chatgpt_base_url`, `mcp_oauth_callback_url`, `otel.*`, `desktop.*`, `allow_login_shell`, `apps.*`, `computer_use.*`, `browser_use.*`, `features.network_proxy.*` (its sub-keys, such as `domains`); `--ignore-user-config`, `--ignore-rules` | **a fleet file may set them; `ctl spawn` refuses them.** |

Every spelling counts: `--sandbox x`, `--sandbox=x`, `-sx`, `-s=x`, and a
`-c` key quoted or not.

The third class is the one the split exists for. Measured with codex 0.163.0:
under a `read-only` sandbox, an MCP server and a `notify` program both wrote to
the home directory, because codex starts them itself, not through the
sandboxed shell. A person writing a fleet file is choosing that for their own
machine, and the file is reviewed before it runs. A `ctl spawn` is written by an
agent, and allowing it there would let a sandboxed agent start a teammate that
runs a program outside the posture the human chose. `features.network_proxy`
itself (`--enable network_proxy`) is allowed from either; it is its sub-keys,
which let network out, that only a fleet file may set.

With `"trust": "skip"` none of this matters: that posture is no sandbox at all.

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
   another agent CLI) or `ctl_allow` in the user-global `config.json`. It never
   weakens a session that didn't set it.
3. **No count cap; a depth guard.** A fleet designed for 32 runs 32; width is
   unlimited. Only *recursion* is bounded, by **`--max-depth`** (default 6, `0`
   removes it): a spawn past the ceiling is refused, so a self-spawning agent
   can't fork-bomb the machine. You always see the live count and can kill any
   subtree.
4. **Subtree-scoped control.** By default an agent may `send` / `status` / `kill`
   / `audit` only panes **in its own subtree**; a lead steers its own team, never
   a sibling's. **You control everything**: a root pane (the one atrium opened, or
   any `ctl` run from outside a pane) is the operator and reaches every pane.
   One deliberate exception: a **bus wake** — the framed line an event becomes
   in the pane of a subscriber or an addressee (`bus pub --to`) — may also go
   *up* the tree, to the panes that spawned the publisher, and to any pane that
   subscribed. It is a sanitized, attributed headline of an event the target
   could read with `bus feed` anyway, capped and coalesced per pane, never free
   text; a worker still cannot reach an unrelated pane by naming it. See [the
   control plane](control-plane.md#the-bus-the-teams-event-stream).
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

## What the mod adds

[The mod](control-plane.md#the-mod-typed-tools-the-role-section-and-subagent-panes) runs inside a pane's Claude Code and speaks to atrium over the same channel as `atrium ctl`, with the same per-pane token. Three more points hold for it.

8. **A report is about the reporter.** `hello`, `report` and `whoami` take no target and act on the token-matched caller; a request that names one is refused by the parser. A pane cannot mark a sibling idle to get a delivery through, nor forge its answer. A reported status is an upgrade over the transcript inference, never a requirement, and stops counting two minutes after it was made.
9. **An answer is data.** `report answer=` is capped (16 KiB), scrubbed of control characters, kept out of the audit log (its length is recorded, never its text), and returned by `answer`/`wait` only up the spawn tree: to the pane's ancestors, and to the pane that spawned it after it has exited. It is never typed into a pane and never shown as operator text.
10. **The mod cannot widen trust.** It turns a permission ask into an allow for its own `atrium_*` tools only, and returns the engine's verdict whenever that is a deny: rules, policy and `ATRIUM_DENY` are never overridden. The mod's folder is written only by `atrium mod install`, from the copy built into the binary, and a fleet or the config may keep it out of every pane with `mod: false`.
11. **An ask is a fork, and its reply is data.** `ctl ask` is subtree-scoped like `send` and queued only for a pane whose mod declared `ask`; the pane answers it from a fork of its own context (no tools, its turn untouched), so a question can never run anything in the pane, and the transcript never leaves it. A reply is accepted only from the pane the question was put to, once, capped (4 KiB), scrubbed, kept out of the audit log (the question's length is recorded, never its text), and readable only by the asker's subtree, for ten minutes. A pane holds at most eight unanswered questions. What a mod reports as `touched` and `doing` is about itself alone: a pane can mislabel its own work, but it cannot plant a collision or a caption on a sibling, and the collision decision names the file and the panes, never any text from either.

See the [project README](../README.md) for the wider picture.
