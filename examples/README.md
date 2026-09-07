# Example fleet: `feature-crew`

[`atrium.fleet.json`](atrium.fleet.json) is a complete, runnable fleet that shows
every field and, more importantly, the shape a fleet is *for*: **one lead that
coordinates a team and drives the work to completion**, a bench of workers, and an
adversarial reviewer. Copy it into a real project, adjust the `cwd`s and the
initiative, and run it.

```bash
cd your-project              # a repo with ./server, ./web, ./docs subdirs
cp path/to/examples/atrium.fleet.json .
atrium fleet ls              # feature-crew
atrium fleet up feature-crew
```

## The one idea: a lead runs the team

A fleet is not five agents doing five things next to each other. That is how work
gets dropped: nobody owns the whole, no part gets reviewed, and a stuck worker
just sits. **One agent is the lead.** It does not implement; it splits the
initiative, assigns one part per teammate, watches progress, sends a finished part
to the reviewer, and only calls a part done once the reviewer clears it. It keeps
going until every part is green.

This works because of how atrium wires a fleet: every fleet agent comes up as an
**operator** pane (no parent in the spawn tree), so any of them may drive any
other over the control plane. The lead can `atrium ctl status api`, `atrium ctl
send web ...`, and read the whole `atrium ctl board list`; the reviewer can be
asked to check any part. The lead role is set entirely by that agent's `prompt`
and `kickoff` — atrium gives every fleet agent the *capability*; the brief decides
who *uses* it as the coordinator.

Two things make the lead actually coordinate rather than just talk about it:

- **`allow_ctl: true`** binds the control plane and injects the `atrium ctl`
  directive into every pane, so the agents know the board and bus exist. Without
  it the panes come up, the kickoffs tell them to publish to a bus that is not
  there, and nothing happens.
- **The coordination skills.** Install them once so a hosted claude reaches for
  `ctl` reliably; the lead prompt here is written in that coordinate-and-delegate
  voice:

  ```
  > /plugin marketplace add nativelite/marketplace
  > /plugin install atrium@nativelite
  ```

## How the crew coordinates

1. The **lead** splits the initiative, then writes each teammate a task with
   `atrium ctl board set <name> ...`.
2. Each **worker** (`api`, `web`, `tests`, `docs`) reads its task with `atrium ctl
   board get <its-name>`, works only that part, and keeps its own board entry
   current so the lead can see status without interrupting it.
3. When a worker finishes, it **publishes on the bus** (`atrium ctl bus pub
   review ...`) and waits.
4. The lead sees the event, asks the **reviewer** to check that exact part, and
   marks it done on the board only when the reviewer publishes a pass.
5. The lead drives this until every part is reviewed and green, then posts a final
   summary and tells the human.

## Field reference (as used here)

Fleet-level:

| Field | In the example | What it does |
| --- | --- | --- |
| `grid` | `"2x3"` | Layout for the six panes. Must fit the agent count; omit it to auto-grid. |
| `identity` | `"work"` | Default credential for every agent that does not name its own. |
| `allow_ctl` | `true` | Brings the control plane up, as `--allow-ctl` does. A coordinating fleet is dead without it. |
| `trust` | `"automode"` | Runs hands-off under claude's own guardrails, so the lead can drive without you clicking dialogs. A request, capped by the session ceiling. |

Per-agent:

| Field | Seen on | What it does |
| --- | --- | --- |
| `name` | all | The roster name; also the pane's role in the bar, board, and bus. Workers key their board entry by it. |
| `cmd` | all | The command and args, e.g. `["claude"]`. |
| `model` | most | `opus` for the lead and reviewer, `sonnet`/`haiku` for cheaper parts. |
| `effort` | most | Reasoning effort per agent (`high` where it matters). |
| `can_spawn` | `lead` | Only the lead may create further teammates with `ctl spawn`. Defaults to `false`; declare it where you want it. |
| `identity` | `reviewer` | Overrides the fleet default (`wif:prod` here) for one agent. |
| `cwd` | workers | Where the agent starts, so its `CLAUDE.md` auto-loads. Relative to this file. |
| `add_dirs` | `api`, `web` | Extra read-only trees (`--add-dir`). Disclosed at launch before you approve. |
| `prompt` | all | The persona, via `--append-system-prompt`. This is where lead vs worker vs reviewer is decided. |
| `kickoff` | all | The first **user** message, so the agent starts the moment the fleet is up. Absent means the agent idles. |

## The one rule for `prompt` and `kickoff`: plain prose

On Windows, atrium launches each agent through `cmd /C claude.cmd`, and the
`prompt`/`kickoff` text is passed as an argument to it. Keep that text to plain
prose — letters, spaces, commas, periods, colons, hyphens. Avoid quotes,
backticks, angle brackets, parentheses, and the shell metacharacters `& | % ^`:
they can break the argument and either mangle the brief or kill the pane. Every
prompt and kickoff in this example follows that rule; when you edit them, keep to
it. (`cmd` args like `["claude"]` are fine — this rule is only about the free-text
brief.)
