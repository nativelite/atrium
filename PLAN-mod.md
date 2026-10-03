# PLAN — the atrium mod: a Claude Code plugin in every pane

Status: design, 2026-10-03, against `b826f79`. Not yet a fleet plan: the item
table at the end is the proposed split. Read `PLAN.md` for the format a running
fleet uses.

Claude Code now loads **mods**: a TypeScript module of function hooks that runs
inside the engine, hooks its events (`tool.check`, `turn.start`, `agent.spawn`,
`prompt.compose`, `session.measure`, …) and calls back into it through `$`
(`$.prompt.submit`, `$.tool.register`, `$.ui.open`, `$.process.run`, …). The
API reference is the `plugin-authoring` skill in Claude Code; its declaration
file is the authority (`types/claude-code.d.ts`, ~20k lines). It is marked early
access and moves between releases.

atrium is in a position no standalone plugin is: it is the **broker every pane's
Claude already talks to** (`ATRIUM_CTL`, `ATRIUM_PANE`, `ATRIUM_TOKEN` are
injected into every pane, `src/pane_spawn.rs:180`), and it owns the chrome the
human looks at. A mod loaded into every claude pane turns three of atrium's
documented approximations into certainties and makes two things possible that
nobody else can do: subagents that are visible panes, and a control plane that
is a typed tool surface instead of shell commands the model has to remember.

## Contents

1. Principles
2. Architecture: one mod, one broker, five new verbs
3. Features, in depth (F1–F9)
4. Security model additions
5. Distribution and version skew
6. Files touched, by side
7. Verification
8. Risks and limits
9. Items (the proposed fleet split)

---

## 1. Principles

- **Upgrade, never require.** Every path keeps working without the mod: codex
  and gemini panes, a claude too old to load mods, a user who declined the
  plugin. The broker treats a mod report as a better signal than agsess, never
  as the only one. agsess remains the fallback and the cross-vendor path.
- **The broker stays the single source of truth.** The mod reports and
  delivers; it never holds state the board and bus do not. A mod reload
  (`register` runs again) must be harmless.
- **Nothing a pane says about another pane is trusted.** A mod authenticates
  with its own `ATRIUM_TOKEN`, and the new verbs act on the caller alone
  (`report`, `inbox`, `hello`) or are subtree-scoped like today's (`wait`).
- **atrium writes nothing new silently.** The mod files land on disk by an
  explicit `atrium mod install`, exactly as `config init` does, and the
  preflight says when they are missing. (README: "The one file atrium does
  write is Claude Code's folder-trust bit, and only under `--trust`.")
- **Feature-detect, pin nothing.** The mod starts with a capability handshake;
  the broker enables each typed path only for a pane whose mod declared it.

## 2. Architecture

```
 ┌─ pane 3: claude (role builder) ──────────────────────────────┐
 │  Claude Code engine                                          │
 │   └─ atrium mod (hooks/register.tsx)                         │
 │        hello ──────────────────────────────┐                 │
 │        tool.check / turn.* / session.* ──► report ──┐        │
 │        $.process.spawn(["atrium","ctl","inbox","--wait"]) ◄──┼── deliveries
 │        $.tool.register(atrium_spawn, atrium_send, …)         │
 │        prompt.compose ◄── whoami (role, item, rules)         │
 │        agent.spawn ──► ctl spawn + wait ◄── child's answer   │
 └──────────────────────────────────────────────────────────────┘
          │ named pipe / unix socket, one JSON line per request
 ┌─ atrium broker (ctl_server) ─────────────────────────────────┐
 │  per pane: caps, reported status, context %, cost, answer    │
 │  status_for(): reported ▸ agsess ▸ unbound                   │
 │  sends/wakes: inbox for a capable pane ▸ pty typing otherwise │
 │  overview / bar / panels read the merged view                │
 └──────────────────────────────────────────────────────────────┘
```

### 2.1 New ctl verbs (`src/ctl/request.rs` `Cmd`, `src/ctl_server/dispatch.rs`)

| verb | who | does | reply |
| --- | --- | --- | --- |
| `hello mod=<ver> engine=<ver> caps=<a,b,c>` | the caller, authenticated | Registers the pane's mod and its capabilities. Idempotent; a reload re-sends it. Caps this design defines: `status`, `inbox`, `answer`, `context`, `tools`, `guard`. | `{"ok":true,"pane":3,"accepted":["status","inbox",…]}` |
| `report status=<s> [reason=<r>]` | caller | The pane's own status, from the engine's events: `working`, `waiting-approval`, `waiting-prompt`, `idle`, `errored`, `ended`. Overrides agsess for this pane until the next report or the mod's `session.end`. | `{"ok":true}` |
| `report context=<pct> [cost=<usd>] [turns=<n>]` | caller | Telemetry from `session.measure`. Stored per pane; shown in the overview. | `{"ok":true}` |
| `report answer=<text>` | caller | The pane's last `turn.complete` text (capped, sanitized), so a parent that spawned it as a subagent can collect it. | `{"ok":true,"seq":n}` |
| `inbox [--wait]` | caller | The caller's undelivered sends and wakes, oldest first. `--wait` holds the connection and streams one JSON line per delivery until the client closes (the mod runs it under `$.process.spawn` for the session's life). The broker marks each delivered when the line is written. | `{"ok":true,"kind":"send"\|"wake","seq":n,"text":"…"}` per line |
| `whoami` | caller | What the pane is: `pane`, `role`, `parent`, `depth`, `mode`, `fleet`, `item` (the board entry the lead briefed it with, if any), `files` (that entry's `files=` field, parsed), `deny`, `topics`, `rules` (the fleet's rules text, see F5). | `{"ok":true,"pane":3,"role":"attention","item":"M1",…}` |
| `wait <target> [--for answer\|exit\|idle] [--timeout <s>]` | subtree-scoped | Blocks until the target reaches the condition; `answer` resolves with the target's last reported answer after its first `idle` since the wait began. Used by F3. | `{"ok":true,"pane":4,"answer":"…"}` |

`Cmd::is_read_only`: `whoami` and `inbox` without `--wait` are read-only;
everything else mutates and requires the token. `report` and `hello` refuse any
`target`: a pane reports for itself only.

### 2.2 Broker state (`src/ctl_server/panes.rs`, new `src/ctl_server/modstate.rs`)

Per pane: `ModInfo { version, engine, caps: BitSet, since: Instant }`,
`Reported { status: Option<(Status, Instant)>, context_pct, cost_usd, turns,
answer: Option<(String, seq)> }`. Pure, unit-tested; cleared on `respawn` and
on the pane's exit.

### 2.3 Status precedence (`src/loop_phases.rs`, `src/vendors.rs`)

`status_for(pane)` becomes: a report younger than `REPORT_STALE` (2 min) →
the reported status; else agsess as today; else unbound. A reported `errored`
and `ended` are new `agsess::Status`-adjacent variants carried in atrium's own
`PaneStatus` enum (do not change the agsess crate for this), mapped for the
bar and overview. `deliver.rs`'s `awaiting_tool` guard still applies to the
pty path; a pane with `inbox` never takes the pty path at all.

## 3. Features

### F1. Certainty status: the yellow border at the moment the prompt appears

**Today.** agsess infers status from the transcript tail: `APPROVAL_DWELL` 7 s,
refresh 1 s active / 5 s quiet (`src/vendors.rs:414`), idle decay 60 s, blind
under auto mode. `docs/agent-status.md:63` promises "a future opt-in hook can
replace the inference with a certainty."

**Mod side.**

```ts
on('session.start', async ($, e, next) => {
  const r = await next(e)
  if (!(await inAtrium($))) return r          // ATRIUM_CTL unset: do nothing
  await ctl($, ['hello', `mod=${MOD_VERSION}`, `engine=${(await $.session.version()).version}`,
               'caps=status,inbox,answer,context,tools,guard'])
  await ctl($, ['report', e.isInteractive ? 'status=waiting-prompt' : 'status=working'])
  startInbox($)                                 // F2
  return r
})
on('turn.start',    async ($, e, next) => { report($, 'working');        return next(e) })
on('tool.check',    async ($, e, next) => {
  const v = await next(e)                       // the engine's verdict: allow | ask | deny
  if (v.decision === 'ask') report($, 'waiting-approval', e.tool)
  return v
})
on('tool.call',     async ($, e, next) => { report($, 'working'); return next(e) }) // the ask was answered
on('turn.complete', async ($, e, next) => {
  const r = await next(e)
  report($, e.reason === 'error' || e.reason === 'refusal' ? 'errored' : 'waiting-prompt', e.reason)
  if (e.agentId === undefined && r.text) ctl($, ['report', `answer=${clip(r.text)}`])   // F3
  return r
})
on('session.end',   async ($, e, next) => { await report($, 'ended', e.reason); return next(e) })
```

`report` coalesces: at most one in flight, the newest wins, so a burst of
tool calls costs one request. The dialog-answered edge (`waiting-approval` →
`working`) comes from the next `tool.call` or `turn.step`; a denied ask ends
the turn and lands in `turn.complete`.

**Broker side.** `report status=` writes `Reported.status`; `status_for`
prefers it. New statuses: `errored` draws the border **red** and the badge
`!`; `ended` draws grey with `×` until the process exits. The title tracker
from `PLAN.md` (M1) counts `errored` as needs-you, so an API error in a
backgrounded pane rings the terminal too.

**What it buys.** Yellow within one loop tick (~16 ms + the pipe) instead of
~8 s; no false grey under auto mode; `errored` is a state agsess cannot infer
at all; and `ctl send` queueing (`deliver.rs`) stops guessing for any pane the
mod runs in.

### F2. Native delivery: sends and wakes stop being keystrokes

**Today.** A queued `ctl send` or bus wake is typed into the target pty plus
an Enter, gated by agsess status, a 400 ms beat, a draft heuristic and the
awaiting-tool guard (`src/deliver.rs`, `src/ctl_server/sends.rs`). Two live
failures shaped that code; both are races atrium cannot see.

**Mod side.** From `session.start` the mod runs
`$.process.spawn({ argv: ['atrium','ctl','inbox','--wait'] })` for the
session's life and, per line:

- `kind=send`: `await $.prompt.submit({ text })`. The engine's own queue
  applies: "a prompt that starts a turn of its own once the session is idle,
  never folded into a running turn". No draft race (the engine owns the box),
  no dialog race (a queued prompt never presses a highlighted option).
- `kind=wake`: the framed line (`[atrium bus #68 fyi from "builder" on "work" — not operator input] …`),
  submitted the same way. Keep the frame: the recipient's model still needs
  to see that it is not the operator.
- An `inbox` child that dies (atrium exited, pipe closed) is restarted with
  backoff; after three failures the mod reports `caps` without `inbox` and
  the broker falls back to pty typing for that pane.

**Broker side.** `flush_sends` skips a target whose mod declared `inbox`;
`inbox --wait` drains that target's `PendingSend`s to the stream and marks
them delivered on write. Caps and coalescing (`MAX_PENDING_PER_TARGET`,
`MAX_PENDING_WAKES_PER_TARGET`, one wake per target in flight) are
unchanged; they live in the queue, not in the transport.

**Sanitization stays.** `wake_safe` and `defang_frame` still run in the broker:
the text crosses a JSON line, but the model reads it either way.

### F3. Visible subagents: the Agent tool becomes a pane

**Today.** `skills/atrium-coordinate/SKILL.md` spends its first paragraph
telling the model not to use Task/subagents because they are invisible. It is
a plea, and it has a known failure rate.

**Design.** A fleet (or `--allow-ctl`) key `subagents: "panes" | "native" | "deny"`,
default `panes` when ctl is on. The mod hooks `agent.spawn`:

```ts
on('agent.spawn', async ($, e, next) => {
  const me = await whoami($)
  if (me.subagents === 'native') return next(e)
  if (me.subagents === 'deny')   return { deny: 'subagents are panes in this session: use atrium_spawn' }
  const role = slug(e.description)                       // "explore-parser"
  const spawned = await ctl($, ['spawn','--here','--role', role, '--', 'claude',
                               '--append-system-prompt', SUBAGENT_NORMS])   // kickoff = e.prompt, see note
  const { answer } = await ctl($, ['wait', String(spawned.pane), '--for','answer','--timeout','1800'])
  $.ui.notice(e.tool_use_id, `ran as atrium pane ${spawned.pane} (${role})`)
  return { deny: `__answer__:${answer}` }                 // see "result relay" below
})
```

Two mechanics to settle in the build, both with a safe fallback:

- **Result relay.** `agent.spawn` resolves `{ model }` or `{ deny }`; it does
  not carry the subagent's answer. Options, in order of preference: (a) the
  mod registers a tool `atrium_subagent` and uses `agent.offer` to hide the
  built-in types and `tool.describe` to point the model at it; the tool's
  `tool.call` handler does the spawn and wait and returns the pane's answer as
  the tool result, so the model experiences exactly what it expects from an
  Agent call; (b) deny with a reason that carries the answer. Build (a); (b)
  is the proof-of-concept.
- **Interactive vs `-p`.** A pane running `claude -p` has no human at the
  prompt and exits on completion; a pane running interactive `claude` with a
  kickoff stays for the human to inspect. Default: interactive, kickoff = the
  prompt, and the pane closes itself on `wait --for answer` unless the fleet
  sets `subagents_keep: true`. Both are visible panes while they run.

The child's mod reports its final answer (`report answer=`, F1), which is what
`wait --for answer` returns. Depth and width stay bounded by `--max-depth`
and the existing spawn policy; a subagent pane is an ordinary child in the
org chart, killable, with a status border.

**Why this stands out.** Every other harness's subagents are opaque. Here the
model's own Agent tool produces a tile the human can watch, zoom into, send
to, and kill, with zero change to how the model is prompted.

### F4. Typed control-plane tools

`$.tool.register` at `session.start`, served by `tool.call` handlers that run
`atrium ctl` through `$.process.run` and return the JSON reply as text:

| tool | schema (required) | maps to |
| --- | --- | --- |
| `atrium_spawn` | `role`, `prompt`; optional `cwd`, `worktree`, `mode`, `model`, `here` | `ctl spawn --role … -- claude …` |
| `atrium_send` | `target`, `text` | `ctl send` |
| `atrium_status` | optional `target` | `ctl status` |
| `atrium_list` | — | `ctl list` |
| `atrium_kill` | `target` | `ctl kill` |
| `atrium_board_set` / `_get` / `_list` / `_claim` / `_release` | `key`, `fields{}` | `ctl board …` |
| `atrium_bus_pub` | `topic`, `fields{}`, optional `decision`, `to[]` | `ctl bus pub` |
| `atrium_bus_feed` / `_resolve` / `_topics` | `since` / `seq` / — | `ctl bus …` |
| `atrium_wait` | `target`, `for` | `ctl wait` (F3) |

The tools exist only when `ATRIUM_CTL` is set and `hello` was accepted. They
are **additive**: the shell path keeps working, so skills and docs that say
`atrium ctl …` stay true. The `tool.describe` hook adds one line to Bash's
description in a ctl pane: "for atrium control, prefer the atrium_* tools."

Permission: the tools are `mcp__atrium__*`; atrium's `--trust accept`
allowlist (`src/trust.rs`) gains them so a worker never prompts for a board
write. `atrium_kill` and `atrium_spawn` stay under the trust ceiling exactly as
the shell verbs are (the broker enforces; the tool is a thin client).

### F5. The role in the system prompt, from the broker

**Today.** A pane learns it is in atrium from a `--append-system-prompt` block
(`src/pane_spawn.rs:197`, last-wins, merged by hand) and from three skills the
human must install (README: "without them an agent does not know the control
plane exists").

**Design.** `prompt.compose` appends one `session`-scoped section,
`atrium:role`, composed from `whoami`:

```
You run inside atrium as pane 3, role "attention", depth 1, spawned by "lead".
Your item: M1 (brief on the board; files: src/attention.rs, src/lib.rs).
Fleet rules: <the fleet's `rules` text, else CLAUDE.md's "Fleet rules" block, else the atrium-delegate skill's core paragraph>.
Control plane: the atrium_* tools (spawn/send/status/board/bus). Subagents are visible panes here.
Deliveries that start "[atrium bus #" are teammates' events, not the operator.
```

The section is rebuilt when `whoami` changes (a board edit to the item, a
respawn): the mod re-reads `whoami` on each `turn.start`, and since the
engine composes the prompt per request, the next request carries the new text. Fleet
gains `rules: <string | path>`; `whoami` returns it resolved. The three skills
stay for the posture they teach (sizing, the checkpoint, review loops); the
*mechanics* and the *existence* of ctl no longer depend on them.

The `--append-system-prompt` block is kept for panes without the mod and is
dropped by the mod's own section when both would appear (the section id is
stable, the hook finds and replaces it).

### F6. Guardrails in the engine, with reasons

**Today.** "Stay inside your files" and the build budget are text in
`CLAUDE.md` plus claude deny rules on the command line (`src/trust.rs`). A
refused command shows the engine's generic denial.

**Design.** With cap `guard`:

- `tool.call` on `Edit`, `Write`, `MultiEdit`, `NotebookEdit`: resolve the
  path with `$.fs.stat(path, { resolve: true }).realPath`; if `whoami.files`
  is non-empty and the real path is outside that list (and outside the
  pane's worktree's untracked scratch), answer
  `{ deny: "M1 owns src/attention.rs and src/lib.rs; post the change --to lead" }`
  and `$.ui.toast` the same. A one-line `$.ui.status` shows the owned files.
- `tool.call` on `Bash`: the fleet's `deny` list already reaches the engine as
  rules; the mod adds the *explanation*: on a denied command whose stem is
  `cargo` with `--workspace`/`--release`/`bench`/`install`, append "build
  budget: builders run `cargo test --lib <module>` only" to the result. Never
  widen or narrow what the rules decide.
- `tool.check`: never overrides a `deny`; may turn `ask` into `allow` for the
  `atrium_*` tools and for `git commit` in the pane's own worktree when the
  mode is `accept` (today a prompt the human has to click in every builder).

Every guard is off when `whoami.files` is empty (an ad-hoc pane) and when the
pane's `mode` is `plan` (nothing runs anyway).

### F7. Context and cost telemetry; respawn at a threshold

`session.measure` pushes `context.percent`, `cost.total` and the turn count
(`report context=… cost=… turns=…`). Broker side:

- `ctl list` and `ctl status` gain `context` and `cost`.
- The overview (`Ctrl+A o`) draws a per-agent context bar (`▂▄▆█`) and a cost
  column; the window aggregate sums cost; the status bar gains a session-wide
  `$1.84` meter (fleet key `show_cost`, default on when any pane reports).
- Fleet key `respawn_at: <pct>` (per agent or default): when a pane crosses
  it **and** is `waiting-prompt`, the broker publishes a bus event
  `--decision --to <its parent> item=<item> msg="context at 84%: checkpoint and respawn?"`
  (never respawns on its own; a respawn mid-item loses work). The lead's
  existing loop already handles it. A pane with no parent (the human's) gets a
  toast through its own mod instead.

This makes the coordinate skill's "keep your own context small" a number on
the screen, and the first harness where the human sees spend per agent live.

### F8. The board and bus inside the pane

`$.ui.open({ id: 'atrium', title: 'atrium' })` on `/atrium` (registered
with `$.command.register`), drawn by `ui.render` on `{ component: 'Pane', requestId: 'atrium' }`
from `ctl board list` + `ctl bus feed` polled every 2 s while open
(`$.clock.every`, cancelled on close):

```
 atrium · pane 3 · attention · depth 1          [r]efresh [c]lose
 ─ decisions ──────────────────────────────────────────────────
  #71 from lead on work  "M1 bigger than M? split?"     [1] resolve
 ─ board ──────────────────────────────────────────────────────
  M1   status=DONE  commit=3cbec20  review=pending   (you)
  M2   status=WIP   owner=filter
  lead phase=2  next="M4"
 ─ work (last 5) ──────────────────────────────────────────────
  #70 fyi builder   item=M2 status=done commit=…
```

Buttons raise `ui.press`; `resolve` runs `ctl bus resolve <seq>`. The band
above the prompt (`AbovePrompt`) shows one line while a wake is queued or a
decision addressed to this pane is open: `◆ 1 decision for you · /atrium`.
The status line (`$.ui.status`) reads `atrium · attention · M1 · ctx 41%`.

The pane opens unasked only on a terminal of 144 columns or more (the engine's
rule) and never for a `-p` run.

### F9. Small, sharp extras

- **Commit trailers.** `attribution.text` appends `Atrium-Item: M1` and
  `Atrium-Pane: attention` to the commit attribution a modded pane writes, so
  `git log --grep` reconstructs a run without the audit log.
- **Faster reaping.** `session.end` → `report status=ended`; the broker closes
  the pane on `prompt_input_exit` without waiting for the pty to drain.
- **Sound.** `$.audio.play` on a decision addressed to the human, honoring the
  `notify` config from `PLAN.md` (`off` silences it). Terminal BEL stays the
  default; this is the desktop-app path where BEL does nothing.
- **Session recovery.** `recover` re-sends `hello`-dependent state: the mod's
  `session.start` fires again on a `--resume`, so a recovered fleet re-reports
  within one tick and the overview is right before the first keystroke.

## 4. Security model additions

The 7-point model in `docs/trust-and-security.md` gains three points.

1. **A report is about the reporter.** `hello`, `report`, `inbox` take no
   target and act on the token-matched caller. A pane cannot mark another pane
   idle to get a delivery through, nor forge a sibling's answer.
2. **An answer is data.** `report answer=` is capped (16 KiB), passed through
   `wake_safe`, and only ever returned to a `wait --for answer` from the pane's
   ancestors (subtree scope, the same rule as `send`). It is never typed into a
   pane and never shown as operator text.
3. **The mod cannot widen trust.** It turns `ask` into `allow` only for the
   `atrium_*` tools and for commits inside the pane's own worktree, and only
   under `accept`; a `deny` from rules, policy or `ATRIUM_DENY` is never
   overridden: `tool.check` fires before the mode settles an ask, and
   `next(e)` resolves to the engine's verdict with rules and policy applied,
   so the mod always returns that verdict when it is `deny` and only ever
   rewrites an `ask`.

Threat added: a malicious mod folder. Mitigated by the mod being installed by
`atrium mod install` from the binary's embedded copy (checksummed) and
`CLAUDE_CODE_PLUGIN_DIRS` pointing only at that folder; a fleet may set
`mod: false` to inject nothing.

## 5. Distribution and version skew

- **Where it lives.** `mod/` in this repo (`.claude-plugin/plugin.json`,
  `hooks/hooks.json`, `hooks/register.tsx`, `hooks/ctl.ts`, `hooks/ui.tsx`,
  `types/index.d.ts`, `hooks/*.test.ts`). Embedded into the binary with
  `include_str!` (it is small text, no third-party code, matching the
  zero-dependency rule).
- **How it lands.** `atrium mod install [--at <dir>]` writes it to
  `<config dir>/mod/<atrium version>/` and records the path in `config.json`
  (`mod.path`), exactly like `config init`. `atrium mod status` shows the
  path, version and whether the host claude can load mods.
- **How a pane loads it.** `pane_base_env` adds
  `CLAUDE_CODE_PLUGIN_DIRS=<path>` for claude panes (and claude aliases) when
  the folder exists and `mod != false`. The engine loads a `--plugin-dir`
  folder for that session only; nothing is written to the user's `~/.claude`.
- **Also on the marketplace.** `atrium@nativelite` gains the mod beside the
  three skills, for people who run `claude` in atrium without `fleet up`.
- **Skew.** `hello` carries `engine=<claude version>`; the mod feature-detects
  each `$` noun it uses (`'tool' in $ && 'register' in $.tool`) and declares
  only the caps whose nouns exist. The broker enables typed paths per cap. A
  version bump of the API that breaks a hook is a skipped hook with a dim
  transcript line (engine behaviour), never a broken pane. `dev.py check`
  runs `claude plugin validate mod/` and `claude plugin test mod/` when a
  `claude` binary is on PATH, and says so when it is not.
- **Preflight.** `fleet up` lists `mod: installed <ver> | missing (run atrium mod install)`
  in the PREFLIGHT block.

## 6. Files touched, by side

**Broker (Rust).**
`src/ctl/request.rs` (five `Cmd`s), `src/ctl/client.rs` (argv → request),
`src/ctl/reply.rs`, `src/ctl_server/dispatch.rs`, new `src/ctl_server/modstate.rs`,
`src/ctl_server/sends.rs` (skip `inbox` targets; stream drain), `src/ctl_server/wake.rs`
(unchanged text, new transport), `src/loop_phases.rs` (status precedence),
`src/vendors.rs` (`PaneStatus` with `Errored`/`Ended`), `src/bar.rs` + `src/theme.rs`
(red border, `!`/`×` badges, cost meter), `src/overview.rs` (context bar, cost),
`src/panels.rs` (nothing: the in-pane view is the mod's), `src/fleet/schema.rs`
(`subagents`, `subagents_keep`, `rules`, `respawn_at`, `show_cost`, `mod`),
`src/config.rs` (`mod.path`), `src/trust.rs` (`mcp__atrium__*` in the accept
allowlist), `src/pane_spawn.rs` (`CLAUDE_CODE_PLUGIN_DIRS`), new `src/mod_cli.rs`
(`atrium mod install|status`), `src/fleet_cli/preflight.rs`, `src/main.rs` (subcommand).

**Mod (TypeScript).** `mod/hooks/register.tsx` (hooks), `mod/hooks/ctl.ts`
(`ctl()` over `$.process.run`, the inbox stream, coalesced `report`),
`mod/hooks/ui.tsx` (pane, band, status), `mod/hooks/tools.ts` (F4 specs and
handlers), `mod/hooks/guard.ts` (F6), `mod/types/index.d.ts` (`$.state`
contract: `inbox`, `whoami`, `board`), tests per feature.

**Docs and skills.** `docs/agent-status.md` (certainty section replaces the
"future hook" line), `docs/control-plane.md` (the verbs, the inbox, visible
subagents), `docs/trust-and-security.md` (§4), `docs/fleets.md` (keys),
README (install the mod; the `atrium_*` tools), `skills/*` (mechanics shrink to
"use the atrium_* tools; the shell verbs are the same"), `CHANGELOG.md`.

## 7. Verification

- **Unit (Rust).** `modstate`: caps parse, stale report expiry, answer cap and
  scope. `dispatch`: `report` refuses a target; `hello` is idempotent; `wait`
  is subtree-scoped and times out. `sends`: an `inbox` target is never typed
  into; the stream marks delivered on write. `loop_phases`: reported beats
  agsess, stale falls back. `trust`: the accept allowlist carries the tools.
  `fleet/schema`: every new key parses, wrong types error naming the key.
- **Mod (`claude plugin test mod/`).** Per feature: status transitions for a
  scripted sequence (`turn.start` → `tool.check` ask → `tool.call` →
  `turn.complete error`); inbox lines become `prompt.submit` calls; the guard
  denies an out-of-scope `Edit` and allows one in scope by real path; the
  `atrium_*` tools map to the right argv; `prompt.compose` carries the role
  section once; `agent.spawn` under `panes` spawns and waits.
- **e2e (`tests/atrium/ctl.rs`, `tests/atrium/session.rs`).** A fake claude
  (the existing test harness's scripted child) that speaks `hello` and
  `report`: the border turns yellow within one tick; a send reaches it over
  `inbox` and never over the pty; a report from pane A about pane B is refused.
- **Live.** `atrium fleet up title` with the mod installed: trigger a
  permission prompt → yellow at once; answer it → clears; kill the API key →
  red `!`; a lead's Agent call → a new tile beside it.
- **Gate.** `python dev.py check` green, including `claude plugin validate mod/`
  when claude is present.

## 8. Risks and limits

- **Early-access API.** Hooks can be renamed between Claude Code releases. The
  cap handshake and feature detection keep a mismatch from breaking a pane;
  the cost is a feature silently going dark, which `atrium mod status` and the
  preflight must surface (`engine X: caps status,inbox; tools missing`).
- **Only claude.** codex and gemini panes stay on agsess. The design never
  removes the pty path.
- **The answer relay (F3) depends on `$.tool.register` + `agent.offer`.** If
  hiding built-in agent types proves unreliable, ship `subagents: "deny"` with
  a reason that names `atrium_spawn`, which already beats today's plea.
- **`-p` panes.** `session.start` says `isInteractive: false`; the mod skips
  the UI (F8) and `inbox` there (a `-p` run takes no prompt), and reports only.
- **Two writers of the system prompt.** The `--append-system-prompt` block and
  `atrium:role` must not both appear; the hook replaces by id. Test it.
- **The 4 MiB `$.store` and 16 KiB answer caps** are design numbers; the build
  measures a real run and adjusts.
- **Windows.** `$.process.spawn` of `atrium ctl inbox --wait` holds a named
  pipe client for the session's life; the server's non-blocking drain must
  tolerate one long-lived client per pane (today every request is one-shot).
  This is the one place the broker's I/O model changes; it gets its own item.

## 9. Items (proposed fleet split)

Sizes follow `atrium-coordinate`: M is 2–5 files or ~300 lines. Anything
touching permissions or the wire protocol is one size up and gets its own
review. Order: N1–N3 in parallel; N4 after N1; N5 after N1+N2; N6 after N1;
N7 after N5; N8 after N4+N6; N9 last.

| item | size | files | done-signal |
|---|---|---|---|
| N1 broker verbs + modstate | M+ (protocol) | `src/ctl/request.rs`, `src/ctl/client.rs`, `src/ctl/reply.rs`, `src/ctl_server/dispatch.rs`, `src/ctl_server/modstate.rs` (new) | `hello`/`report`/`whoami`/`wait` parse, dispatch, refuse a target; `cargo test --lib -- ctl modstate` green |
| N2 mod skeleton + status (F1) | M | `mod/**` (new), `dev.py` (validate/test when claude present) | `claude plugin validate mod/` clean; status-transition tests green |
| N3 fleet + config keys, `atrium mod install`, env injection | M | `src/fleet/schema.rs`, `src/config.rs`, `src/mod_cli.rs` (new), `src/pane_spawn.rs`, `src/main.rs`, `src/fleet_cli/preflight.rs` | keys parse; install writes once and is idempotent; `CLAUDE_CODE_PLUGIN_DIRS` set only for claude panes with the folder present |
| N4 status precedence + chrome | M | `src/loop_phases.rs`, `src/vendors.rs`, `src/bar.rs`, `src/theme.rs`, `src/overview.rs` | reported beats agsess; `errored` red `!`; overview context/cost columns; tests green |
| N5 inbox transport (F2) | M+ (I/O model) | `src/ctl_server/sends.rs`, `src/ctl_server.rs`, `src/ipc/*` (long-lived client), `mod/hooks/ctl.ts` | an inbox target is never typed into; stream drains and marks delivered; Windows named-pipe long client holds; e2e green |
| N6 typed tools + role section + guard (F4–F6) | M | `mod/hooks/tools.ts`, `mod/hooks/guard.ts`, `mod/hooks/register.tsx`, `src/trust.rs` | tools map to argv; role section appears once; guard denies by real path; accept allowlist carries `mcp__atrium__*` |
| N7 visible subagents (F3) | M+ (new surface) | `mod/hooks/agents.ts`, `src/ctl_server/dispatch.rs` (`wait --for answer`), `src/fleet/schema.rs` (`subagents*`) | an Agent call under `panes` opens a tile and returns the pane's answer; `deny` mode names `atrium_spawn` |
| N8 telemetry + respawn decision + in-pane UI (F7, F8) | M | `mod/hooks/ui.tsx`, `src/ctl_server/dispatch.rs` (`respawn_at` decision), `src/overview.rs` | `/atrium` pane draws board and decisions; crossing `respawn_at` posts one decision to the parent; status bar cost meter |
| N9 docs, skills, changelog, e2e | M | README, `docs/*.md`, `skills/*`, `CHANGELOG.md`, `tests/atrium/ctl.rs`, `tests/atrium/session.rs` | `python docs/build.py`; e2e fake-claude `hello`/`report`/`inbox` green; integrator's full gate green |

---

# Part II — Reach

Part I makes atrium's existing promises exact. Part II is what becomes
possible once every pane has a mod and the broker can hear it. Each entry
names the mechanism it stands on; everything named here exists in the
engine's declaration file or in atrium today. Sizes are rough; the order of
attack is at the end.

What atrium holds that nobody else does: a **vterm emulator per pane** (the
screen of every agent), a **spawn tree** with trust ceilings, **worktrees** per
worker, **akey identities** per pane, a **board, a bus and an audit log** in
one process, **session snapshots** for recovery, and now a **voice inside every
engine**. Combine them.

## A. Sight

**R1. Captions: what each agent is doing, in six words.**
The mod hooks `turn.step` (the streaming model request) and, every N seconds
of streaming, runs `$.model.classify` or a tiny `$.model.complete` on the last
few hundred characters of the model's own text with "what is this agent doing
right now, six words". It reports `doing="running the filter tests"`. The
overview (`Ctrl+A o`) and the tile border show the caption beside the status
glyph. Mission control gets subtitles. Cost: one haiku-sized call per caption,
rate-limited by the mod, off under `captions: false`.

**R2. Peek: read a worker's screen.**
`ctl peek <target> [--rows N]` returns the target's vterm screen as text.
atrium already renders every pane's emulator; this exposes it over ctl,
subtree-scoped like `send`. A lead reads a worker's failing test output
without a transcript; a reviewer checks what the builder actually ran.
*Security:* a screen can show conversation text, which atrium today never
exposes. So `peek` is behind a session flag (`--allow-peek`), is disclosed at
`fleet up` like the directory grant, and is written to the audit log.

**R3. Ask an agent without interrupting it.**
`ctl ask <target> "<question>"` is delivered to the target's mod, which runs
`$.model.fork({ prompt })`: one tool-less completion over that session's own
transcript, as the engine already does for its own forks. The answer comes
back over `report`. The worker's turn is untouched, its context is not
appended to, and its transcript never leaves its pane. `Ctrl+A ?` on a tile
asks "what are you doing, what do you need, what is blocking you" and shows
the answer as an overlay. This is the one that makes a 12-pane fleet feel
like a team you can talk to.

**R4. Collision radar.**
Mods report the real path of every `Edit`, `Write` and `MultiEdit` at
`turn.complete` (`report touched=…`). The broker keeps a live map of file →
{pane, branch, first touch, last touch}. Two panes on different worktrees
touching the same file lights a `⚡` on both tiles, lists them under
"collisions" in the overview, and posts one `--decision --to lead` the first
time. `ctl who <path>` answers "who has this file open". The integrator gets
the list before it merges instead of after.

**R5. Flight recorder and replay.**
Every report, bus event, board write and audit row already has a timestamp;
add a vterm screen snapshot per pane every few seconds (text, diffed, cheap)
and write the run as one append-only file under the session store.
`atrium replay <run>` scrubs the timeline: every tile's screen at time T, the
board as it was, the decisions open. `atrium fleet resume --from <t>` rebuilds
the fleet from that moment: worktrees are already on disk, the board and bus
reload from the record, and each pane comes back with `claude --resume` on the
transcript it had. A three-hour run becomes a thing you can rewind.

## B. Voice

**R6. The model's own messaging, routed through atrium.**
Claude Code's `SendMessage` and `ListAgents` fire `session.send`; the mod
readdresses `to: "dev_1"` into `ctl send` and feeds `ctl inbox` lines into
`session.receive`. The model uses the tool it already knows; atrium gets the
audit, the rate cap, the subtree scope and the no-echo rule it already
enforces. `ListAgents` lists the pane's visible subtree.

**R7. A shared live document.**
A board key `doc:<name>` holds Markdown. The mod draws it in a pane
(`Markdown` element) in every pane that opened it, re-rendered on every board
write (2 s poll, or a bus event `doc:<name>`). Agents write it with
`board set`; the human edits it in place (`Input`) and the write goes back to
the board. A whiteboard the whole fleet and the human share, with no file to
fight over.

**R8. Guided operation.**
In the lead's and the human's panes the mod uses `prompt.suggest` after a bus
event: a builder posts `status=done`, and the lead's prompt box shows, dim,
`atrium_send reviewer "review M1: commit 3cbec20"` with Tab to take it. The
coordinate skill's loop becomes a sequence of Tabs when the human is driving.

**R9. Review inside the pane.**
The reviewer's mod renders the item's diff (`Code` with `format: 'diff'`)
beside the brief, with `[p]ass` and `[f]ail` buttons that write the board and
post to the bus. The human can be the reviewer from any pane, in the
overview, with two keys.

## C. Control

**R10. Approvals routed to wherever you are.**
`tool.check` resolving to `ask` is the permission dialog. Instead of waiting in
a pane the human is not looking at, the mod sends the ask to the broker and
waits (bounded; on timeout the local dialog shows as today). The broker shows
the ask in the human's *focused* pane as a band with hotkeys (`y`/`n`/`a` for
always), in the overview, and on the phone (R19). The answer returns as the
hook's `{ decision }`. The trust ladder is respected: a lead may answer a
worker's ask only if the lead's own ceiling allows that action; otherwise it
is the human's. Two live-run reliefs: "approve for all siblings running the
same command" and "deny once, add to the session deny list" (R11).

**R11. Deny propagation.**
A denial in one pane offers, by toast with a button, to add the rule to the
session deny list that `--trust` already distributes; the broker records it
and every pane's mod applies it in `tool.check` at once.

**R12. Budgets.**
Fleet keys `budget_usd` (whole fleet) and per-agent `budget_usd`, fed by
`session.measure`. At 80% the broker posts a decision; at 100% it refuses new
spawns and, under `budget: hard`, sends a checkpoint-and-stop to each pane
and then kills. The status bar shows `$12.40 / $40`. The first terminal
multiplexer with a money kill-switch.

**R13. Credential failover.**
`session.measure` reports rate-limit windows. When a pane's window passes
`identity_failover_at` (say 95%), the broker respawns that pane in place
under the next identity in its `identities: ["work", "work2"]` list, through
akey as today, and posts an fyi. A fleet rides out a rate limit across
accounts without a human. Nobody else has per-pane credential injection to do
this with.

**R14. Fork a worker.**
`ctl spawn --fork-of dev_1 --role dev_1b` launches
`claude --resume <dev_1's session> --fork-session` in a new pane: a copy of a
worker's full context at this moment, on a fresh worktree. Branch an agent the
way you branch a repo. Pair with R21 to race three forks with three hints and
keep the winner.

## D. Memory

**R15. Compaction-safe fleets.**
The mod hooks `session.compact`: before the engine compacts, it writes the
checkpoint to the board (what it is doing, what is open, the files touched)
and, on the way back, prepends the role section and the board's state to the
summary. A pane that compacts mid-item loses nothing the fleet needs. The
broker can also *trigger* a compaction (`ctl compact <target>`, via
`$.session.compact`) as the gentle alternative to `respawn_at`.

**R16. The right fact reaches the right agent.**
A hazard or decision posted on the bus carries `files=`. The broker appends it
with `$.session.append` (a meta row the model reads at its next turn, no wake)
only into panes whose owned or touched files overlap. No message storm; the
agent that needs the fact has it before its next edit.

**R17. Memory across runs.**
The board's final state and every hazard are stored per project under the
session store. The next fleet's `whoami` carries "last run: open items,
hazards, what was reverted", so a Monday lead starts where Friday's stopped.

## E. Reach

**R18. Any Claude Code joins the fleet.**
A `claude` started outside atrium (a VS Code terminal, a plain shell) with the
mod installed finds a running broker through atrium's session registry and
asks to join. atrium shows the request in the bar (`join? vscode:claude · y/n`);
on yes the session appears in the overview as a *remote* pane: status, captions,
sends, bus, approvals, no tile. The documented v1 limitation ("an agent you
started inside a shell pane is not bound") is gone, and atrium becomes mission
control for every Claude Code on the machine.

**R19. The phone.**
Claude Code can attach a mobile surface to a session (`session.attach`,
`surface: "mobile"`). One anchor pane's mod draws the fleet lobby for it:
every pane's status and caption, the open decisions with resolve buttons, the
pending approvals (R10) with approve/deny, the budget meter. Walk away from
the desk; the fleet keeps going and asks you on your phone.

**R20. Chrome for any process.**
`atrium ctl run --role tests --ok 'test result: ok' --fail 'FAILED|error' -- cargo watch -x test`
opens a non-agent pane whose border color follows regexes over its own vterm
output. A test watcher, a dev server, a tail of a log: green, red, or grey at a
glance, in the same grid as the agents. The status model stops being
agent-only.

## F. Science

**R21. Fleet bench.**
`atrium fleet bench <fleet> --runs 3 --vary lead.model=opus,sonnet` runs the
same PLAN under each variant and prints a table: wall time, cost per agent and
total, turns, review pass rate, collisions, respawns. Each run is a flight
record (R5). atrium becomes the harness for comparing multi-agent setups,
which today nobody can do except by feel.

**R22. Prompt experiments.**
The fleet's `rules` and each agent's `prompt` are `prompt.compose` sections;
the bench varies them (`--vary builder.rules=@rules-a.md,@rules-b.md`). A/B
your agent instructions with numbers.

**R23. The tool EKG.**
The mod pane draws a `Raster` heat map: panes down, minutes across, cell
brightness = tool calls, colored by kind. An agent that runs the same command
six times in a row is flagged `looping` by the broker and surfaces as a
decision. Loops are the most expensive failure in long runs and today they are
invisible until the bill.

## Three tasks this makes possible

**The overnight refactor, from your phone.** A twelve-pane fleet with
`budget_usd: 40`, `identities` failover, approvals routed (R10) to the lobby
on your phone (R19), collision radar (R4) and compaction-safe panes (R15).
You answer three approvals and one decision before bed. In the morning the
flight record (R5) replays the run in four minutes, the bench table (R21) says
what it cost per item, and the integrator has already merged what passed
review.

**The living map.** Six builders and one indexer pane. The indexer reads
captions (R1) and touched-file reports (R4) off the bus and keeps a shared
document (R7) current: what each part of the codebase is becoming, who owns
it this hour, which hazards were posted. The human watches the map draw itself
and asks any pane a question without interrupting it (R3).

**Fork and race.** A worker is stuck. Fork it three times (R14) with three
different hints in the kickoff, run them as a bench (R21), watch the EKG (R23)
for the one that loops, kill the losers from the overview, merge the winner.
Twenty minutes instead of an afternoon.

## Order of attack

After Part I's N1–N9. Groups are independent of each other; within a group
the order is the dependency order.

| group | items | why first |
|---|---|---|
| see | R1 captions, R3 ask, R4 collision radar | pure additions on `report`; the biggest legibility gain per line |
| guard | R12 budgets, R10 approvals, R11 deny propagation | the unattended-run enablers; R10 touches trust and gets its own review |
| remember | R15 compaction-safe, R16 targeted injection, R5 flight recorder | R5 is the largest single item in Part II |
| reach | R18 join, R19 phone, R20 any process | R18 and R19 change who atrium is for |
| voice | R6 native messaging, R7 shared doc, R8 suggest, R9 review pane | polish on top of F8 |
| science | R21 bench, R22 experiments, R23 EKG | needs R5 and F7 |
| branch | R13 failover, R14 fork | small, each one unique to atrium |

Every item keeps Part I's principles: upgrade never require, the broker stays
the source of truth, nothing a pane says about another pane is trusted,
nothing is written silently, and a feature that the engine of the day cannot
serve goes dark with a visible line, never a broken pane.
