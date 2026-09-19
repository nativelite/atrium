# PLAN — terminal title and "needs you" notifications

This is the fleet's plan and every agent's restart point. The lead owns it and
the board; a builder reads its item here and the brief on the board. Items are
built one per fresh session, checkpointed, reviewed fresh, merged one at a time
by the integrator, who runs the only full gate and pushes `main` after each
green merge. M4 starts after M1 and M3 are merged; M5 after M4.

## Items

| item | size | owner | files | done-signal |
|---|---|---|---|---|
| M1 attention module | M | attention | `src/attention.rs` (new), `src/lib.rs` | module + 10 unit tests green (`cargo test --lib attention`) |
| M2 filter strips child titles | M | filter | `src/filter.rs` | 4 filter tests green; OSC 52/8 still pass |
| M3 config + fleet keys | M | config | `src/config.rs`, `src/fleet.rs`, the four `Fleet` literals (`fleet.rs:2273`, `fleet.rs:2541`, `fleet_cli.rs:1481`, `worktree.rs:522`) | `notify`/`title` parse, wrong types error, `apply_to_fleet` fills; tests green |
| M4 plumbing | M (after M1+M3 merge) | fresh builder | `src/main.rs` (`run` arg, startup push, tick before `renderer.paint`, teardown reset), `src/loop_phases.rs` (`attention_facts`), `src/fleet_cli.rs` (Setup from fleet) | builds; `cargo test --lib` green; title visible in a live launch |
| M5 docs + e2e + changelog | M (after M4 merge) | fresh builder | README, `docs/agent-status.md`, `docs/fleets.md`, `docs/control-plane.md`, `CHANGELOG.md`, `tests/atrium.rs` | `python docs/build.py`; e2e `host_title_names_the_project_and_is_reset_on_quit` green; integrator's full gate green |


## Context

When atrium is in a background tab or window, nothing tells the operator what the fleet is doing or that an agent is blocked on them. The status bar already knows (`?` marker, `N waiting`, `N decisions need you`), but only when you are looking. The feature surfaces that outside the pane: the host terminal's title names the session and summarizes state, and a transition into "needs you" rings the terminal.

Two latent defects get fixed on the way: atrium never owns the title, and in passthrough mode a child's own `OSC 0/2` title escape leaks straight to the host (`src/filter.rs:176-178`), so claude's tab title and atrium's would fight.

Decisions taken with the founder (2026-09-18): channel default **bell + toast**; notify on **needs-you only** (title changes for everything); name = **fleet name, else project directory**, fleet may override with `title`.

## Design

**Title** (`OSC 2`, change-gated): `atrium · <name> · <state>`
- `? builder needs you · 2 working` / `? 2 need you` / `? 1 decision needs you`
- `● 3 working`
- `○ idle`
Precedence needs-you > working > idle, the bar's own ladder. `Idle | WaitingPrompt | unbound` count as idle; exited panes are neither.

**Notify** on transitions only: a pane newly `WaitingApproval` (by pane key, 30 s cooldown per pane) or a newly pending human-addressed decision (by bus `seq`, so resolve-then-new is not hidden by an equal count). Channels: `BEL`; `OSC 9;<text>` toast (iTerm2/ConEmu); and for Windows Terminal, which ignores plain OSC 9, the tab progress indicator `OSC 9;4;2;0` (error/red) while `needs() > 0` and `OSC 9;4;0` to clear — persistent on the tab, cleared when handled. Config `notify`: `both` (default) | `bell` | `toast` | `off`, in `config.json` and per fleet.

**Plumbing**: a pure `attention` module; the loop computes a `Summary` every tick from the same facts `bar_infos` uses and writes the tracker's bytes to `out` right before `renderer.paint` (`src/main.rs:~2711`) — same `Screen` chunk as the frame, one write on Windows, fires on an idle tick when the frame is empty. Title pushed at startup (`CSI 22;2 t`), reset at teardown (empty `OSC 2` then `CSI 23;2 t`).

## Edits (ordered)

1. **`src/attention.rs` (new) + `pub mod attention;` in `src/lib.rs`.** API:
   - `enum Notify { Both, Bell, Toast, Off }` — `parse`, `bell()`, `toast()`, `DEFAULT = Both`.
   - `struct Setup { name: String, notify: Notify }`; `Setup::project_dir_name()` (cwd basename, `atrium` fallback).
   - `struct PaneFact { key: u64, label: String, exited: bool, status: Option<agsess::Status> }`.
   - `struct Summary { needs_you: Vec<(u64, String)>, decisions: Vec<u64>, working, idle, exited }`; `Summary::build(&[PaneFact], human_decisions: &[u64])`; `needs()`.
   - `state_text(&Summary)`, `title_text(name, &Summary)`, `text_safe(&str)` (control chars incl. BEL/ESC/C1, U+2028/9, bidi/zero-width → space; cap 200 chars — same list as `ctl_server::wake_safe`).
   - `TITLE_PUSH = "\x1b[22;2t"`, `TITLE_RESET = "\x1b]2;\x07\x1b[23;2t"`, `title_osc(text)`, `bell()`, `toast_osc(text)`, `progress_osc(on: bool)` (`\x1b]9;4;2;0\x07` / `\x1b]9;4;0;0\x07`).
   - `enum Event { PaneNeedsYou { key, label }, DecisionNeedsYou { seq } }`; `struct Edges { cooldown_ms, last_rang }` with `diff(prev, next, now_ms) -> Vec<Event>` (`COOLDOWN_MS = 30_000`).
   - `struct Tracker { setup, prev, edges, last_title, progress_on }` with `tick(next: Summary, now_ms) -> Vec<u8>`: title OSC iff changed; on events and notify≠Off, one BEL and/or one toast (`atrium: builder needs you` / `atrium: 2 need you`); progress on/off iff the needs state flipped and toast is enabled.
2. **`src/filter.rs`**: `enum Osc { Strip(usize), Hold, No }` + `fn osc(rest)` beside `xtwinops` (`:97-119`): `ESC ]` Ps ∈ {0,1,2} (whole number; `10` ≠ `1`) → strip through `BEL`/`ESC \`; other Ps → pass (52 clipboard, 8 links); unterminated < 512 bytes → hold; over → pass. Wire into `feed` after the xtwinops arm (`:144-154`); update the module doc. Note ConPTY re-emits a child's title as `OSC 0`.
3. **`src/config.rs`**: `GlobalConfig.notify: Option<Notify>`; parse `"notify"` via `opt_str` + `Notify::parse`, error naming the four values; `apply_to_fleet` fills `fleet.notify`.
4. **`src/fleet.rs`**: `Fleet.title: Option<String>`, `Fleet.notify: Option<Notify>`; parse in `parse_fleet` beside `identity` (`:326-333`); add to the constructor and the four `Fleet { .. }` literals (`fleet.rs:2273`, `fleet.rs:2541`, `fleet_cli.rs:1481`, `worktree.rs:522`).
5. **`src/main.rs`**: `run()` gains `attention: atrium::attention::Setup`; startup write (`:2068`) prefixed with `TITLE_PUSH`; `let mut attention = Tracker::new(setup)` after `Renderer::new()` (`:2251`); before `renderer.paint` (`:2711`): build facts (`attention_facts`), human decisions (`bus.pending_decisions()` filtered by `decision_for_agent`, `ctl_server.rs:296-303`, mapped to `seq`), `attention.tick(Summary::build(..), now_ms)`, `out.write_all` if non-empty. `cleanup_screen` (`:2876-2885`): `TITLE_RESET` after `\x1b[r`, before `?1049l`. Callers `:659` (plain) and `:1826` (recover): `Setup { name: Setup::project_dir_name(), notify: config::get().notify.unwrap_or(DEFAULT) }`.
6. **`src/loop_phases.rs`**: `attention_facts(windows, world) -> Vec<PaneFact>` beside `bar_infos` (`:359`): key = `p.agent_id.0` (verify uniqueness across windows; else `(window_idx << 32) | p.id`), label = `role` else `title`, `exited`, `status = world.status_for(session_id)`.
7. **`src/fleet_cli.rs:1084-1097`**: pass `Setup { name: fleet.title.clone().unwrap_or(name), notify: fleet.notify.unwrap_or(DEFAULT) }` (after `apply_to_fleet`).
8. **Docs**: README config block + bullet for `notify`; short "Title and notifications" note; `docs/agent-status.md` new section after "### Status bar" (format, precedence, channels, cooldown, WT/tmux caveats); `docs/fleets.md` keys `title`, `notify`; `docs/control-plane.md` ~:81 "and rings the terminal"; `CHANGELOG.md` `[Unreleased]` → `### Added`. Rebuild `docs/build.py`.

## Verification

- Unit (`src/attention.rs`): `summary_counts_panes_by_status`; `title_prefers_needs_you_then_working_then_idle` (table incl. 2 waiting, 1 decision, waiting+decision); `title_text_scrubs_control_characters_and_caps_length`; `diff_rings_only_on_a_newly_waiting_pane`; `diff_honors_the_per_pane_cooldown`; `diff_rings_for_a_newly_pending_human_decision` (`{1}→{2}` rings); `tracker_writes_the_title_only_when_it_changes`; `tracker_channels_follow_notify` (Both/Bell/Toast/Off byte contents, progress on/off flips once); `escapes_are_exact`; `notify_parses_its_four_values_and_rejects_others`.
- `src/filter.rs`: `a_panes_title_change_is_stripped` (OSC 0/1/2, BEL and ST terminated); `osc_52_and_osc_8_pass_through`; `title_split_across_chunks_is_still_stripped`; `osc_10_is_not_osc_1`.
- `src/config.rs` / `src/fleet.rs`: `notify` parses, garbage names the key, `apply_to_fleet` fills; `title`/`notify` fleet keys parse and wrong types error.
- e2e (`tests/atrium.rs`): `host_title_names_the_project_and_is_reset_on_quit` — spawn a shell session, `read_until` `\x1b]2;atrium` (`\x1b]0;atrium` on Windows: ConPTY re-emits as OSC 0), assert a payload contains the cwd basename, `^A q`, then the empty-title reset appears and the process exits. `strip_csi_bytes` leaves `ESC ]` alone so `contains` works.
- Gate: `python dev.py check` green; live: launch `atrium` in a project → tab title reads `atrium · <dir> · ○ idle`; in a fleet, trigger a permission prompt → tab flashes/rings within ~8 s and the title shows `? <role> needs you`; answer it → title clears, WT tab indicator clears.

## Risks / known limits (documented in agent-status.md)

- Latency: `APPROVAL_DWELL_MS` 7 s + agent refresh 1 s/5 s → a permission prompt rings ~8 s after it appears; working→idle lags `IDLE_MS` 60 s.
- Windows Terminal: no OSC 9 toast (BEL + the `9;4` tab indicator are the signals); ConPTY has no title stack, the empty-title reset restores the tab name.
- tmux: OSC 2 sets the pane title (outer title with `set-titles on`); OSC 9 needs `allow-passthrough`; BEL follows `bell-action`.
- First tick after `recover`/`fleet up` of an already-waiting agent rings once (prev is empty) — intended.
- Nested atrium: the inner one's title is now stripped by the outer filter (intended).
