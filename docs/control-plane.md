# The control plane (ctl)

A [fleet](fleets.md) is a *saved* org chart; **`ctl`** builds one **live**, and lets an agent (or you) *drive* it. With `--allow-ctl`, a pane can **spawn**, **send** to, **observe**, and **kill** other agent panes, turning atrium from a viewer of agents into a **runtime** for them.

## What ctl is

The point of doing this inside atrium is legibility. Every agent ctl creates is a visible, killable pane with a status border, listed in the org chart. Because the whole hierarchy is on screen and controllable, atrium coordination stays legible in a way opaque subagents are not.

The shape it is built for is one human, a couple of coordinators, and a wide bench of workers:

```
  you (a shell/agent pane)
   └─ hand an initiative to →
      lead_a, lead_b          (coordinators)
        └─ each splits it across →
           dev_1, dev_2, …    (workers, each its own pane)
```

Trust posture (how hands-off spawned agents run, and the ceiling they are capped at) is set with `--trust` and is covered separately; see [trust and security](trust-and-security.md).

## Turning it on

The control plane is **opt-in and off by default**. Without `--allow-ctl` the channel is not bound and `atrium ctl` refuses; a session that did not ask for it is unchanged.

```bash
atrium --allow-ctl claude                 # bind the control channel
atrium --allow-ctl --trust claude         # …and launch every agent trusted + hands-off
atrium --allow-ctl --max-depth 3 claude   # cap spawn recursion (default 6; 0 = unlimited)
```

`--max-depth <N>` bounds spawn recursion so a self-spawning agent cannot fork-bomb the machine. The default is `6`; `0` removes the cap. Width is never capped, only recursion depth.

When ctl is on, atrium opens a **per-instance control channel** (a **named pipe** on Windows, a **unix socket** elsewhere; zero-dep, non-blocking-drained in the run loop) and injects two env vars into **every** pane it spawns:

- **`ATRIUM_CTL`**: the channel endpoint. `atrium ctl` connects here.
- **`ATRIUM_PANE`**: the caller pane's agent id, so the server attributes each request to its place in the spawn tree.

Any process in a pane (a shell command, or an agent via a tool or hook) issues control by running **`atrium ctl <cmd>`**, which speaks one JSON request per line and prints the one-line JSON reply.

## The command surface

Targets are a pane **id** (its numeric agent id) or a **role** label (`dev_1`); an ambiguous role is a clear error.

| Command | Does | Reply |
| --- | --- | --- |
| `atrium ctl spawn [--role R] [--identity X] [--here\|--window] [--mode <policy>] -- <cmd…>` | Open a visible worker running `<cmd>` (a new window, or `--here` tiled beside the caller), tagged `R`, optionally under credential identity `X`. `--mode <plan\|accept\|automode\|skip>` sets that worker's trust posture, capped at the session ceiling (see [trust](trust-and-security.md)). | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `atrium ctl list` | The live org chart: every pane with parent, role, depth, and `agsess` status. | `{"ok":true,"tree":[{"id":0,"parent":null,"role":null,"title":"claude","depth":0,"status":"idle"},…]}` |
| `atrium ctl send <target> <text…>` | Deliver `<text>` to a pane's agent as a submitted prompt. **Queued until the target is idle** (agsess-gated), so it never lands mid-turn. | `{"ok":true,"target":3,"queued":true}` |
| `atrium ctl status [<target>]` | One target's status, or (no target) a roll-up of the caller's subtree. Backed by `agsess`: `working` / `waiting-approval` / `waiting-prompt` / `idle` (or `null` if unbound); a pane's mod reports it for certain (`errored`, `ended` too) and adds its caption as `doing` while fresh. | `{"ok":true,"pane":3,"status":"working","idle_ms":0,"doing":"running cargo test --lib ctl"}` |
| `atrium ctl kill <target>` | Terminate a worker **and its whole subtree** (a lead's kill reaps its ICs). The dead panes' windows re-tile or close on the next tick. | `{"ok":true,"killed":[3,4,5]}` |
| `atrium ctl respawn <target> [--worktree <name>]` | Relaunch a pane's process **in place** as a new session: kill the old child and start its own command again (arguments and kickoff included; for claude, any `--resume`/`--continue`/`--session-id` is dropped so the conversation starts fresh), keeping the pane's role, parent, depth, mode, deny rules, and context store. With `--worktree <name>` the new process starts in that (idempotently created) git worktree; otherwise it restarts where it was. Its workers stay its workers and sends queued for it reach the new process. A pane may respawn itself — a lead uses this to clear its context. | `{"ok":true,"pane":3,"role":"dev_1","session":"…"}` |
| `atrium ctl audit [N]` | The control-request log (most recent `N`, or all), for reconstructing a run. | `{"ok":true,"audit":[{"seq":1,"caller":0,"action":"spawn","detail":"role=dev_1 argv=claude identity=-","ok":true,"note":"pane=1"},…]}` |
| `atrium ctl board set <key> <field=value…>` | Merge fields into the shared board (create if absent); an empty value clears a field. | `{"ok":true,"key":"auth","entry":{"by":"dev_1","ms":…,"fields":{"status":"DONE","owner":"Max"}}}` |
| `atrium ctl board get <key>` / `list` / `del <key>` | Read one entry, roll up the whole board, or remove an entry. | `{"ok":true,"board":[{"key":"auth","by":"dev_1","fields":{"status":"DONE"}},…]}` |
| `atrium ctl board claim <key> [--ttl <secs>]` / `release <key>` | Atomically claim a key as a **lease** (owner is the caller, derived server-side; default TTL unless `--ttl`), or release it. A claim is granted only if the key is free or its lease expired; otherwise the reply names the current holder and how long its lease runs. | `{"ok":true,"key":"auth","granted":true,"holder":"dev_1","lease_ms":…,"entry":{…}}` |
| `atrium ctl bus pub <topic> [--decision] [--to <role\|id>[,…]] [--new] <field=value…>` | Publish a structured event to `topic`. Default urgency `fyi`; `--decision` marks an escalation that needs a human answer. `--to` addresses it to panes by role or id; addressed panes and the topic's subscribers are **woken** (below). `--new` opens a topic nobody has used, when the fleet declares no vocabulary. | `{"ok":true,"event":{"seq":7,"topic":"deploy","kind":"fyi","from":"dev_1","fields":{"msg":"merged"}}}` |
| `atrium ctl bus sub <topic…>` / `unsub [<topic…>]` | Subscribe (merged; `*` = firehose) or unsubscribe (empty ⇒ all) the caller. Subscribing opts you into wakes: an event on that topic is typed into your pane once you are idle. | `{"ok":true,"subscribed":["deploy","*"]}` |
| `atrium ctl bus feed [--since <seq>]` | Pull the caller's subscribed events with `seq > since` (no echo of your own); returns a `cursor` to resume from. | `{"ok":true,"feed":[{"seq":7,"topic":"deploy",…}],"cursor":7}` |
| `atrium ctl bus resolve <seq>` | Mark a `decision_needed` event answered (clears the bar/panel escalation). | `{"ok":true,"seq":7,"resolved":true}` |
| `atrium ctl bus topics` | List the active topics with how many panes subscribe to each — discoverability before you `sub`. | `{"ok":true,"topics":[{"topic":"deploy","subs":2},…]}` |
| `atrium ctl hello mod=<v> [engine=<v>] [caps=<a,b,…>]` | A pane's mod announces itself; the broker answers with the capabilities it accepts. About the caller only. | `{"ok":true,"pane":3,"accepted":["status","answer",…]}` |
| `atrium ctl report [status=<s>] [reason=<r>] [context=<pct>] [cost=<usd>] [turns=<n>] [answer=<text>] [doing=<caption>] [touched=<path>]… [ask=<id> reply=<text>]` | A pane's mod reports its own status, usage or last answer; its **caption** (`doing`, a few words on what it is doing now; empty clears it); the real paths it **touched** with an edit (one `touched=` per path); or its **reply** to a queued question. About the caller only; a `target` is refused, and a reply is accepted only for an ask put to that pane. | `{"ok":true,"pane":3}` |
| `atrium ctl whoami` | What the caller is: pane, role, parent, depth, mode, worktree, cwd, deny rules, whether it may spawn, its mod, the board item whose `owner`/`builder` names its role and that item's `files`, and the session's subagent policy. | `{"ok":true,"pane":3,"role":"builder",…,"item":"M1","files":[…],"subagents":"panes"}` |
| `atrium ctl inbox` | The caller's queued `send`s and bus wakes, handed over and removed from the queue, for a pane whose mod declared `inbox`: it submits them through the engine's own prompt queue, and atrium types nothing into that pane. About the caller only; an empty poll leaves no audit row. | `{"ok":true,"items":[{"kind":"send","text":"…"},{"kind":"wake","text":"[atrium bus #…] …"}]}` |
| `atrium ctl answer <target>` | A pane's last reported answer and status, for the ancestor that spawned it (subtree-scoped like `send`; a pane that answered and exited stays readable by its parent). | `{"ok":true,"pane":4,"status":"waiting-prompt","seq":2,"answer":"…"}` |
| `atrium ctl wait <target> [--for answer\|idle\|exit] [--timeout <s>] [--after <seq>]` | Poll until the pane has an answer newer than `seq`, is at its prompt, or is gone. A client-side loop over `answer`/`status`; an empty poll leaves no audit row. | the last `answer`/`status` reply, or `{"ok":false,"err":"timeout after 300s …"}` |
| `atrium ctl ask <target> [--timeout <s>] <question…>` | Ask a pane a question **without interrupting it**: the question is queued, the pane's mod takes it with its inbox and answers from a fork of the pane's own context (one tool-less completion over its transcript, which never leaves the pane), and the reply comes back over `report`. Subtree-scoped like `send`; the target's mod must have declared `ask`. The client polls `asked` for the reply (default 120 s, at most 600). | `{"ok":true,"pane":3,"id":7,"reply":"Running the filter tests; nothing blocks me."}` or `{"ok":false,"err":"timeout after 120s …; read it later with atrium ctl asked 3 7"}` |
| `atrium ctl asked <target> <id>` | The reply to an earlier ask, once given (`null` while pending); readable by the asker's subtree for ten minutes, and by the parent after the pane exited. | `{"ok":true,"pane":3,"id":7,"reply":"…"}` |
| `atrium ctl who <path>` | Which panes edited a file, from their mods' `touched` reports: the pane, its role, when it first and last touched the file, and whether it is still here. `path` is the real path, or one relative to a worktree (`src/x.rs` finds the file in every worktree), or a trailing part of either. Read-only, like `list`. | `{"ok":true,"path":"src/x.rs","panes":[{"pane":2,"role":"dev_1","key":"src/x.rs","path":"/w/a/src/x.rs","first_ms":…,"last_ms":…,"live":true}]}` |

## The board: shared source of truth

`send` is the ephemeral message stream ("I just shipped auth"); the **board** is the durable current truth (`auth: DONE, owner: Max, url: …`). It answers *"what is currently true?"*.

It is a schemaless **`key → fields`** tracker living in the atrium daemon. Because atrium is the single broker process every pane talks to, it is a plain map behind the pipe: **single writer, no locking, no consensus.** A coordinating lead sets tasks and reads `board list` for status/owner/blocker instead of re-scraping each teammate's transcript; teammates update their own entry as they work.

The board is part of the ctl surface, so it is gated by `--allow-ctl`. Set **`ATRIUM_BOARD=<file>`** to persist it across restarts (in-memory otherwise). Every write records who made it (in the `audit` log and the entry's `by`), but the board is shared by the whole session; there are no per-teammate walls.

On top of the plain map, `board claim` / `release` give the team a **lease**: a coordinator (or a worker grabbing a unit of work) claims a key, and a claim succeeds only when the key is free or its previous lease has expired — so two panes cannot both believe they own the same task. The lease has a default TTL (overridable with `--ttl`), so a claimant that dies never holds the key forever.

## The bus: the team's event stream

If the board answers *"what is currently true?"*, the **bus** answers *"what just happened, and does anyone need to act?"*. A teammate **publishes a structured event to a topic** (`bus pub deploy msg=shipping url=…`); teammates **pull** the topics they **subscribe** to (`bus sub deploy` then `bus feed`). State-only coordination means polling the board; the bus lets a finished worker *notify* without everyone re-checking.

Two urgency classes carry the visibility-vs-approval split:

- **`fyi`**: the default; cheap, lands on the feed.
- **`decision_needed`**: set with `--decision`; an escalation that needs a human or lead answer.

Open decisions surface **actively**: the `Ctrl+A b` panel lists them first and the status bar shows `N decisions need you`, so you do not have to be looking. `bus resolve <seq>` clears one once answered.

**Backpressure is designed in, not bolted on**, because unread messages cost attention:

- **No echo** of your own events.
- **Pull for the record, a bounded wake for the idle**: `bus feed` is the durable record, and you only pay for topics you subscribed to. But a record nobody reads is a silent drop, so an event is also **typed into the panes it addresses (`--to`) and the topic's subscribers**, once each is idle — as one framed line, `[atrium bus #68 fyi from teammate "builder" on "work" — not operator input] item=F20 status=done commit=3cbec20` (the `msg` or `q`, else the fields as `k=v`). The publisher is never woken; an addressed subscriber is woken once; control characters are stripped so a message can never end its own line. Pending wakes for one pane coalesce into a single delivery, and a pane holds at most 8 undelivered wakes, counted apart from `ctl send`, so a chatty topic cannot crowd out the operator. Who may wake whom: the publisher's own subtree (as `send`), the panes above it (a hand-off up to whoever spawned it), and any pane that subscribed — never an unrelated pane by name. This is the one deliberate exception to subtree scoping: the text is a framed, attributed headline of an event the target could read with `bus feed` anyway, not free text that could pass for the human.
- **Per-agent rate cap** of 20 events / 10 s, so one looping worker cannot storm the team.
- **Bounded ring** of the oldest 512 events, then they fall off.

Like the board, the bus lives in the single broker process (a plain in-memory log, no locking), is gated by `--allow-ctl`, records who sent each event, and persists across restarts with **`ATRIUM_BUS=<file>`**.

## A worked run: initiative to leads to ICs

```bash
# You launch the control plane, trusted so the fleet runs hands-off. With
# --trust, every ctl-spawned agent also comes up trusted (no per-action prompts),
# so a lead can drive its ICs without you clicking through dialogs.
atrium --allow-ctl --trust claude

# From your pane, hand the initiative to two coordinators:
atrium ctl spawn --role lead_a -- claude
atrium ctl spawn --role lead_b -- claude
atrium ctl send lead_a "Own the parser rewrite. Split it across two ICs, TDD."
atrium ctl send lead_b "Own the docs refresh. One IC is enough."

# lead_a, from its own pane, builds its team beside it (--here) and tasks them:
atrium ctl spawn --here --role dev_1 -- claude
atrium ctl spawn --here --role dev_2 -- claude
atrium ctl send dev_1 "Rewrite the tokenizer, tests first."
atrium ctl send dev_2 "Rewrite the AST builder, tests first."

# lead_a coordinates by status, not by scraping terminals:
atrium ctl status            # roll-up of lead_a's own subtree
atrium ctl status dev_1      # one IC (lead_b's team is out of scope, refused)

# When an IC is done, its lead reaps it (and anything under it):
atrium ctl kill dev_1

# You see all of it, any time: the org chart and the full trail:
atrium ctl list
atrium ctl audit
```

The multiplier: many workers, coordinated by a few of them, driven by one human, and every level is a pane you can watch, redirect, zoom into (`Ctrl+A z`), or kill. `ctl` is the small verb layer; the panes, tiling, identity injection, and `agsess` status it stands on already existed.

## The mod: typed tools, the role section and subagent panes

With `atrium mod install` done, every claude pane carries a plugin (the mod, `mod/`) that speaks the verbs above for it and gives the model three things:

- **Typed tools.** `atrium_spawn`, `atrium_send`, `atrium_status`, `atrium_list`, `atrium_kill`, `atrium_board_set|get|list|claim|release` and `atrium_bus_pub|feed|resolve|topics`: each call is one `atrium ctl` run with the pane's own token, and the reply is what the model reads. The shell verbs stay the same surface, so the skills and the docs remain true either way. A permission ask for one of these tools is answered by the mod, never a dialog; a deny from a rule stands.
- **The role section.** An `atrium:role` section of the system prompt, from `whoami`: the pane, its role, parent, depth and trust mode, its item on the board and the files that item owns, its worktree, whether it may spawn, and what a `[atrium bus #` line is. Re-read every turn, so a lead's re-brief lands. When the item owns files, an `Edit`, `Write` or `NotebookEdit` anywhere else is refused by real path with the owner named.
- **Native delivery.** A pane whose mod declared `inbox` is never typed into: its sends and wakes wait in the queue until the mod collects them with `inbox` and submits each through the engine's prompt queue, which starts a turn only when the session is idle. The draft and dialog races the pty path guards against (`src/deliver.rs`) do not arise, and a task may span lines. A pane without the cap keeps the pty path.
- **The `/atrium` pane.** Inside Claude Code, `/atrium` opens a pane with the open decisions (each with a resolve button and a hotkey), the board, and the last bus events the pane subscribes to, refreshed every 3 s while open; a band above the prompt counts the open decisions and opens the pane. With a fleet's `respawn_at`, a pane whose context fill crosses the threshold while idle gets one `decision_needed` posted to its parent on the `atrium` topic, with the parent woken: the lead's "respawn when heavy" rule becomes a number it is asked about.
- **Sight: captions, the collision radar and asks.** The mod reports a **caption** on every tool call, free of charge (`running cargo test --lib ctl`, `editing filter.rs`, `reading main.rs`) and, unless the fleet says `captions: false`, a six-word caption from the model's own words every twenty seconds or so of streaming (one small `haiku` call); the overview shows it in place of the transcript's last action and the tile draws it dim after the title. It reports the real path of every `Edit`, `Write` and `NotebookEdit` that ran, and the broker keys each by the pane's worktree, so two worktrees' copies of `src/x.rs` are one file: the first time two live panes edit it, both tiles and overview rows wear `⚡`, the overview lists the file under the agents, and one `collision` `decision_needed` goes to the nearest pane above both (the lead) on the `atrium` topic, or to the human when there is none. `atrium ctl who <path>` answers who has a file, before the merge. And `atrium ctl ask <target> <question>` (or `Ctrl+A ?` on a tile, or `atrium_ask` from a pane) puts a question to a pane's mod, which answers from a fork of the pane's own context: the turn is untouched, nothing is appended to its context, and the transcript never leaves the pane; the reply opens as an overlay for the human and returns as the tool's result for an agent.
- **Subagents as panes.** Under the session's `subagents` policy (`panes`, the default; a fleet may say `native` or `deny`), the model's Agent tool is refused with a pointer to `atrium_subagent`, which spawns a claude pane beside the caller (`--here`), sends it the task, waits for the answer the child's own mod reports, closes the pane unless `keep` or the fleet's `subagents_keep`, and returns the answer. Every subagent is a tile the human can watch, send to and kill.

What the broker believes of a mod is bounded: a report is about the reporter (a `target` is refused), an answer is capped, scrubbed and read only up the spawn tree, and a mod cannot widen trust (it rewrites an ask to allow for its own tools only; a deny is never overridden). See [trust and security](trust-and-security.md).

## Delegation skills: atrium-delegate, atrium-coordinate and atrium-fleet

`ctl` is the mechanism; three Claude Code skills (in `skills/`) are the instruction layer that makes an agent *use* it. Copy the directories into `~/.claude/skills/` and any atrium-hosted claude picks them up. The first two exist because the trigger is **who decides to delegate**:

- **`atrium-delegate`** (ambient / discretionary): the agent *may* hand off an independent, substantial part when it genuinely pays, and does small or dependent work inline. On its own a capable agent usually judges it can do the work itself (which is correct), so this fires rarely by design. Its real value is discoverability: without it, an agent does not know `atrium ctl` exists at all.
- **`atrium-coordinate`** (directed): for when you *want* fan-out. It casts the agent as a coordinator that splits the work, delegates **every** part, monitors, collects, and reaps, instead of implementing inline. This is the reliable lever: on a task where the ambient skill chose inline every time, coordinate mode fanned out every time.

The third covers the saved-fleet path rather than ad-hoc delegation:

- **`atrium-fleet`**: designing, configuring and running an `atrium.fleet.json` — the file's full schema, what the resource budget actually costs (compiling is the scarce resource), what atrium enforces, and how to read the `PREFLIGHT` block.

Drop a spawned lead straight into coordinate mode by telling it to coordinate:

```bash
atrium ctl spawn --role lead -- claude
atrium ctl send lead "Coordinate this across a team, one teammate per part: <the initiative>"
```

or, from your own pane, just say "coordinate this across a team: …" and let the skill surface.

## See also

- [README](../README.md): atrium overview and the full feature set.
- [trust and security](trust-and-security.md): the `--trust` ladder and the ctl security model.
- [fleets](fleets.md): saved rosters (`atrium fleet up`), the persisted org chart.
- [credential identity](identity.md): per-pane credentials via `--identity` and `akey`.
