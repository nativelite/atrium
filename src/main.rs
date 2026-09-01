//! The amux binary: a dual-mode run loop — passthrough (0.1) and tiled (0.2).
//!
//!     amux                      # panes run your shell (COMSPEC / $SHELL)
//!     amux claude               # panes run `claude`
//!     amux claude --continue    # any command + args
//!
//! Ctrl+A then: c new window, n/p cycle, 1-9 switch windows, x kill focused
//! pane, q quit; `"`/`%` split the focused pane, h/j/k/l or arrows move focus,
//! z zoom (full-screen passthrough) the focused pane.
//!
//! ## The two rendering modes
//!
//! A window whose split tree is a single pane — or any window with its focused
//! pane *zoomed* — renders in **passthrough**: the pane's raw VT bytes go
//! straight to the real terminal, zero emulation, perfect fidelity. This is the
//! 0.1 path, untouched.
//!
//! The moment a window holds two or more visible panes it renders **tiled**:
//! each pane drives a `vterm::Term` sized to its rect, every pane's screen is
//! composited into one master `ansi::Screen`, and `Screen::diff` writes only the
//! changed bytes. Zoom is the escape hatch back to perfect fidelity for a TUI
//! that the emulator can't render pixel-exact (wide glyphs, sixel, mouse).

use amux::bar::{bar_paint, PaneInfo};
use amux::input::{Action, Dir, PrefixScanner};
use amux::layout::{self, Rect, Tree};
use amux::tile::{compose, AgentMark, PaneState, PaneView};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A process-global, monotonic **agent id** stamped on every pane amux hosts —
/// the stable key of the ctl spawn tree (`Pane.id` is only unique within a
/// window; this is unique across the whole run). Incremented once per spawn.
static NEXT_AGENT: AtomicUsize = AtomicUsize::new(0);

/// The ctl channel address, set once at startup iff `--allow-ctl` was given.
/// The spawn path reads it to inject `AMUX_CTL`/`AMUX_PANE` into each pane so an
/// agent inside can drive `amux ctl`. `None` (unset) ⇒ ctl is off and every
/// spawn is byte-identical to pre-ctl amux.
static CTL_ADDRESS: OnceLock<String> = OnceLock::new();

/// Set once at startup from the `--trust` / `--skip-permissions` flags: how much
/// amux relaxes the permission posture of every agent pane it spawns. Encoded as
/// `0=Off`, `1=Edits` (`--trust`: acceptEdits + safe allowlist), `2=Skip`
/// (`--skip-permissions`: full bypass). Read by the spawn path
/// (`spawn_pane_full`) via [`trust_mode`].
static AGENT_TRUST: AtomicU8 = AtomicU8::new(0);

/// Decode [`AGENT_TRUST`] into the typed mode.
fn trust_mode() -> amux::ctl::TrustMode {
    match AGENT_TRUST.load(Ordering::Relaxed) {
        1 => amux::ctl::TrustMode::Edits,
        2 => amux::ctl::TrustMode::Skip,
        3 => amux::ctl::TrustMode::Plan,
        4 => amux::ctl::TrustMode::Auto,
        _ => amux::ctl::TrustMode::Off,
    }
}

/// Publish the launch trust mode to the spawn path.
fn set_trust_mode(m: amux::ctl::TrustMode) {
    let code = match m {
        amux::ctl::TrustMode::Off => 0,
        amux::ctl::TrustMode::Edits => 1,
        amux::ctl::TrustMode::Skip => 2,
        amux::ctl::TrustMode::Plan => 3,
        amux::ctl::TrustMode::Auto => 4,
    };
    AGENT_TRUST.store(code, Ordering::Relaxed);
}

fn next_agent_id() -> usize {
    NEXT_AGENT.fetch_add(1, Ordering::Relaxed)
}

/// DEC synchronized-output (private mode 2026). amux wraps each composited frame
/// it emits to the *real* terminal in these markers, so the outer terminal paints
/// the whole frame (all tiled panes + the bar) atomically instead of showing it
/// half-drawn — that half-drawn frame is the tiled "shutter". This is the emit
/// side of the same mode `vterm` honors on the way in; a terminal that doesn't
/// support 2026 ignores the markers, so it degrades cleanly.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Max reads per active pane per tick (each up to the 8 KiB `buf`). High enough
/// to drain a whole frame from a big burst before compositing (~0.5 MiB), so we
/// never composite a half-drawn pane; the drain still breaks early the instant
/// the pane has no more data, so this cap only matters on a genuine flood.
const DRAIN_READS_PER_TICK: usize = 64;

/// Draw the animated startup splash for a passthrough pane that has not painted
/// yet — a centered `a m u x` wordmark and a spinner, so the agent's boot reads
/// as *loading*, not a hang. Written straight to the terminal and wrapped in
/// synchronized output so each frame is atomic (no flicker from the per-frame
/// clear); the drain wipes it on the pane's first real bytes.
/// True while a pane's emulator has produced no *visible* content yet — every
/// cell is default. Used to keep the startup splash up until the agent actually
/// paints (its terminal-setup bytes arrive first and must not count as painted).
/// Short-circuits, so it is cheap and only fully scans during the brief boot.
fn term_blank(t: &vterm::Term) -> bool {
    let s = t.screen();
    let blank = ansi::Cell::default();
    for r in 0..s.rows() {
        for c in 0..s.cols() {
            if s.cell(r, c) != blank {
                return false;
            }
        }
    }
    true
}

/// True if `bytes` contains a full-screen erase (`ESC[2J` or `ESC[3J`). Unlike a
/// scroll, `2J`/`3J` ignore the scroll region and wipe the whole screen — the bar
/// row included — so amux must repaint the bar after one.
fn clears_screen(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|w| w == b"\x1b[2J" || w == b"\x1b[3J")
}

/// Sniff a pty output chunk for the app turning mouse tracking on/off — a
/// DECSET/DECRST for private mode 1000/1002/1003 (`ESC [ ? … h` / `… l`). Returns
/// `Some(true)` if the chunk last enabled mouse, `Some(false)` if it last
/// disabled it, `None` if it touched neither. Handles combined params
/// (`ESC[?1002;1006h`). Best-effort and stateless: a sequence split across two
/// reads may be missed, but apps emit these as a single write at startup/teardown
/// so it reliably tracks whether a pane wants the wheel. Drives `Pane::mouse_wanted`.
fn sniff_mouse_mode(buf: &[u8]) -> Option<bool> {
    let mut result = None;
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let start = i + 3;
            let mut j = start;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && (buf[j] == b'h' || buf[j] == b'l') {
                let is_mouse = std::str::from_utf8(&buf[start..j])
                    .ok()
                    .map(|s| s.split(';').any(|p| matches!(p, "1000" | "1002" | "1003")))
                    .unwrap_or(false);
                if is_mouse {
                    result = Some(buf[j] == b'h');
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

/// Render the full-screen coordination overlay (`Ctrl+A b`): the shared **board**
/// (durable "what is true") on top, a divider, then the **bus** feed (recent
/// events + any open `decision_needed` escalations, "what just happened") below.
/// Hides the cursor, clears, and positions every line with absolute CUP (no
/// scrolling); the bar row is left for the status bar.
fn render_board_panel(
    board: &amux::board::Board,
    bus: &amux::bus::Bus,
    rows: u16,
    cols: u16,
) -> String {
    let mut out = String::from("\x1b[?25l\x1b[2J");
    let decisions = bus.pending_decisions();
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1;38;5;37m  board + bus\x1b[0m  \x1b[2m(Ctrl+A b to close · live)\x1b[0m{}",
        if decisions.is_empty() {
            String::new()
        } else {
            format!(
                "  \x1b[1;38;5;11m{} decision{} awaiting you\x1b[0m",
                decisions.len(),
                if decisions.len() == 1 { "" } else { "s" }
            )
        }
    ));
    out.push_str(&format!(
        "\x1b[2;1H\x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat(cols as usize)
    ));

    // Split the content rows (3..=bar-1) between the board (top) and the feed
    // (bottom half). Defensive on tiny terminals: sections shrink, never overrun.
    let content_last = rows.saturating_sub(1); // the bar lives on row `rows`
    let region = content_last.saturating_sub(2); // rows 3..=content_last
    let feed_h = (region / 2).clamp(3.min(region), region);
    let board_last = content_last.saturating_sub(feed_h + 1).max(3);

    render_board_rows(&mut out, board, board_last);

    // The bus divider + feed, filling from `board_last + 2` down to the bar.
    let div_row = board_last + 1;
    out.push_str(&format!(
        "\x1b[{div_row};1H\x1b[38;5;238m\u{2500}\u{2500} \x1b[0m\x1b[1;38;5;37mbus\x1b[0m \x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat((cols as usize).saturating_sub(7))
    ));
    render_feed_rows(&mut out, bus, &decisions, div_row + 1, content_last);
    out
}

/// Draw the board entries into `out`, from row 3 down to `last_row` (inclusive),
/// with an overflow hint if there are more than fit.
fn render_board_rows(out: &mut String, board: &amux::board::Board, last_row: u16) {
    use amux::ctl::{hyperlink, is_url, status_glyph, status_sgr};
    let entries = board.list();
    if entries.is_empty() {
        out.push_str(
            "\x1b[3;1H  \x1b[2m(board empty — set one:  amux ctl board set launch status=WIP owner=you)\x1b[0m",
        );
        return;
    }
    let key_w = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(4).clamp(4, 24);
    let mut row = 3u16;
    let total = entries.len();
    for (i, (key, e)) in entries.iter().enumerate() {
        if row > last_row {
            out.push_str(&format!(
                "\x1b[{row};1H  \x1b[2m… {} more (resize taller)\x1b[0m",
                total - i
            ));
            break;
        }
        let status = e.fields.get("status").map(String::as_str);
        let color = status.map(status_sgr).unwrap_or("\x1b[0m");
        let glyph = status.map(status_glyph).unwrap_or("\u{00B7}");
        let status_label = status.map(|st| format!("{color}{st}\x1b[0m  ")).unwrap_or_default();
        let mut fields = String::new();
        for (f, v) in &e.fields {
            if f == "status" {
                continue;
            }
            let rendered = if is_url(v) { hyperlink(v, v) } else { v.clone() };
            fields.push_str(&format!("\x1b[2m{f}=\x1b[0m{rendered}  "));
        }
        let by = e
            .updated_by
            .as_deref()
            .map(|b| format!("\x1b[2m(by {b})\x1b[0m"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\x1b[{row};1H  {color}{glyph}\x1b[0m \x1b[1m{key:<kw$}\x1b[0m  {status_label}{fields}{by}",
            kw = key_w,
        ));
        row += 1;
    }
}

/// Draw the bus feed into `out`, from `first_row` down to `last_row`: open
/// `decision_needed` escalations first (amber `!`), then recent FYI events
/// (newest first, dim `·`). URLs render as clickable OSC-8 links.
fn render_feed_rows(
    out: &mut String,
    bus: &amux::bus::Bus,
    decisions: &[&amux::bus::Event],
    first_row: u16,
    last_row: u16,
) {
    if first_row > last_row {
        return;
    }
    let cap = (last_row - first_row + 1) as usize;
    // Open decisions get priority; fill the rest with recent non-open events,
    // newest first, skipping any already shown as an open decision.
    let open_seqs: std::collections::BTreeSet<u64> = decisions.iter().map(|e| e.seq).collect();
    let mut lines: Vec<String> = decisions.iter().map(|e| feed_line(e)).collect();
    if lines.len() < cap {
        for e in bus.tail(cap * 2).into_iter().rev() {
            if lines.len() >= cap {
                break;
            }
            if open_seqs.contains(&e.seq) {
                continue;
            }
            lines.push(feed_line(e));
        }
    }
    if lines.is_empty() {
        out.push_str(&format!(
            "\x1b[{first_row};1H  \x1b[2m(no events — publish one:  amux ctl bus pub deploy msg=shipping)\x1b[0m"
        ));
        return;
    }
    for (i, line) in lines.into_iter().take(cap).enumerate() {
        let row = first_row + i as u16;
        out.push_str(&format!("\x1b[{row};1H{line}"));
    }
}

/// One bus event as a panel line: `! deploy  msg=ship it?  (from dev_1)`. A
/// `decision_needed` event gets an amber `!` and bold topic; an FYI a dim `·`.
fn feed_line(e: &amux::bus::Event) -> String {
    use amux::bus::Kind;
    use amux::ctl::{hyperlink, is_url};
    let (glyph, topic_sgr) = match e.kind {
        Kind::DecisionNeeded => ("\x1b[1;38;5;11m!\x1b[0m", "\x1b[1;38;5;11m"),
        Kind::Fyi => ("\x1b[2m·\x1b[0m", "\x1b[1m"),
    };
    let fields = e
        .fields
        .iter()
        .map(|(f, v)| {
            let rendered = if is_url(v) { hyperlink(v, v) } else { v.clone() };
            format!("\x1b[2m{f}=\x1b[0m{rendered}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    let from = e
        .from
        .as_deref()
        .map(|f| format!("  \x1b[2m(from {f})\x1b[0m"))
        .unwrap_or_default();
    format!("  {glyph} {topic_sgr}{}\x1b[0m  {fields}{from}", e.topic)
}

fn draw_startup_splash(out: &mut impl std::io::Write, rows: u16, cols: u16, frame: usize) {
    const SPIN: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
    let (rows, cols) = (rows as usize, cols as usize);
    if rows < 3 || cols < 10 {
        return;
    }
    let spin = SPIN[frame % SPIN.len()];
    let brand = "a m u x";
    // The wordmark with a cool per-letter gradient (cyan → azure → indigo →
    // violet), the same palette as the themed chrome. Escapes don't count toward
    // width, so the visible run is still exactly `brand` (7 cols) — `bcol` below
    // centers on that.
    let mut brand_colored = String::new();
    for (i, ch) in ['a', 'm', 'u', 'x'].iter().enumerate() {
        if i > 0 {
            brand_colored.push_str("\x1b[0m "); // plain space between letters
        }
        let (r, g, b) = amux::theme::SPLASH_GRADIENT[i];
        brand_colored.push_str(&format!("\x1b[1;38;2;{r};{g};{b}m{ch}"));
    }
    brand_colored.push_str("\x1b[0m");
    let sub = format!("{spin}  starting your agent…  {spin}");
    let mid = (rows / 2).max(1);
    let bcol = (cols.saturating_sub(brand.chars().count()) / 2) + 1;
    let scol = (cols.saturating_sub(sub.chars().count()) / 2) + 1;
    // Appended into the tick's single synchronized frame (with the bar), NOT a
    // frame of its own — so the whole screen (wordmark *and* bar) repaints
    // atomically. Drawing the splash on its own write path used to `2J`-clear the
    // bar a beat before the bar's separate frame repainted it, which read as the
    // white bar flashing on startup. No `?2026`/flush here: the caller wraps the
    // composite in `SYNC_BEGIN`/`SYNC_END` and flushes once. `?25l` hides the
    // cursor so it doesn't blink next to the spinner; the agent restores it
    // (`?25h`) at the handoff in the drain.
    let _ = write!(
        out,
        "\x1b[?25l\x1b[2J\x1b[{mid};{bcol}H{brand_colored}\
         \x1b[{};{scol}H\x1b[2;36m{sub}\x1b[0m",
        mid + 1
    );
}

/// A `ctl send` awaiting delivery. The design queues a task until the target is
/// **idle** (agsess-gated) rather than injecting into a live turn (Decision 4).
/// Once the target is ready we write the text, then — after a short beat so the
/// agent's TUI registers the line before the submit — write the Enter. The beat
/// mirrors the C0 spike, which split the text and `\r` with a delay.
struct PendingSend {
    /// Target pane's global agent id (already resolved + scope-checked).
    target: usize,
    text: String,
    /// When the send was accepted — a fallback so a target that never yields a
    /// derivable status (a shell, a not-yet-bound agent) still gets it.
    queued_at: Instant,
    /// `Some(t)` once the text has been written; `t` gates the follow-up Enter.
    text_written_at: Option<Instant>,
}

/// How long after writing the task text we send the Enter that submits it.
const SEND_ENTER_DELAY: Duration = Duration::from_millis(400);
/// If a target never yields a derivable agsess status (non-agent / unbound),
/// deliver anyway once the send has waited this long, so a queue never wedges.
const SEND_UNBOUND_FALLBACK: Duration = Duration::from_secs(2);

/// Max height (rows) of a spurious "shrink" amux ignores in the resize poll: the
/// Windows Terminal ConPTY reports a few rows fewer once amux is on the alternate
/// screen, and adopting it leaves a strip of stale content below the bar. A
/// height-only shrink no larger than this is treated as that reservation, not a
/// real resize. Generous enough to cover the observed reserve, small enough that a
/// genuine window resize (usually larger, and changing width) is still honored.
const ALT_SCREEN_RESERVE_ROWS: u16 = 8;

/// Find a hosted pane by its global agent id (immutable / mutable).
fn pane_by_agent(windows: &[Window], id: usize) -> Option<&Pane> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .find(|p| p.agent_id == id)
}
fn pane_by_agent_mut(windows: &mut [Window], id: usize) -> Option<&mut Pane> {
    windows
        .iter_mut()
        .flat_map(|w| w.panes.iter_mut())
        .find(|p| p.agent_id == id)
}

/// `(agent_id, role)` for every live pane — the candidate set for target
/// resolution.
fn ctl_candidates(windows: &[Window]) -> Vec<(usize, Option<String>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.role.clone()))
        .collect()
}

/// `(agent_id, parent)` for every live pane — the spawn-tree edges the subtree
/// guard walks.
fn ctl_parents(windows: &[Window]) -> Vec<(usize, Option<usize>)> {
    windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .map(|p| (p.agent_id, p.parent))
        .collect()
}

/// The **human** controls everything (Decision 3). A caller is human-privileged
/// when it has no attributed pane, or when its pane is a *root* (one amux opened,
/// `parent == None`, depth 0) — i.e. where the operator sits. A spawned worker
/// (depth > 0) is scoped to its own subtree.
fn caller_privileged(windows: &[Window], caller: Option<usize>) -> bool {
    match caller {
        None => true,
        Some(id) => match pane_by_agent(windows, id) {
            Some(p) => p.parent.is_none(),
            None => true, // unknown caller (e.g. run from outside a pane) = operator
        },
    }
}

/// One hosted terminal: a pty, its emulator (for tiled compositing), its
/// passthrough filter (for the passthrough / zoom path), and bar metadata. Each
/// pane has a stable `id` the window's split tree refers to.
struct Pane {
    id: usize,
    pty: pty::Pty,
    term: vterm::Term,
    filter: amux::filter::Passthrough,
    title: String,
    activity: bool,
    exited: bool,
    /// The session id amux injected via `--session-id` when this pane is an
    /// agent it launched (§3.3). `None` for shells and agents amux did not
    /// bind. The binder maps this to the pane's live `agsess::Status`.
    session_id: Option<String>,
    /// The credential identity **name** this pane's agent runs under, if any
    /// (path B). Only the name is stored — never the resolved secret. On every
    /// spawn amux re-resolves the env via `akey` and injects it for that child
    /// alone; the resolved values live only for the spawn call and are dropped
    /// immediately. Surfaced in the chrome as a `·<name>` tag.
    identity: Option<String>,
    /// Process-global agent id (see [`NEXT_AGENT`]): the ctl spawn-tree key,
    /// stable across windows. Injected into the pane as `AMUX_PANE` so an agent
    /// inside can attribute its own `ctl spawn` calls.
    agent_id: usize,
    /// The ctl role label this pane was spawned under (`dev_1`), if any. `None`
    /// for the human's own panes and shells.
    role: Option<String>,
    /// The agent id of the pane whose `ctl spawn` created this one. `None` for a
    /// root pane the human opened.
    parent: Option<usize>,
    /// Depth in the spawn tree: 0 for a root/human pane, parent.depth + 1 for a
    /// ctl-spawned worker. The `--max-depth` recursion guard is checked against
    /// this.
    depth: usize,
    /// True once the pane's process has produced any output. Until then, a
    /// *passthrough* (single/zoomed) pane shows the animated startup splash
    /// instead of a blank screen (the tiled path uses a per-pane blank check in
    /// the compositor). Set on the pane's first byte in the drain.
    painted: bool,
    /// True while this pane's app has mouse tracking enabled (it emitted a
    /// DECSET 1000/1002/1003), sniffed from its output. Scroll-wheel notches are
    /// only forwarded to a pane that wants mouse — so hovering a claude tile
    /// scrolls it, while a bare shell never receives stray mouse bytes.
    mouse_wanted: bool,
}

/// One window: a split tree over a set of panes, plus a zoom flag. Windows are
/// the 0.1 "switchable full-screen" concept; each can now itself be a tiled
/// split tree (design doc §3: "windows and splits coexist").
struct Window {
    panes: Vec<Pane>,
    tree: Tree,
    zoomed: bool,
    next_id: usize,
}

impl Window {
    fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    fn focused_mut(&mut self) -> Option<&mut Pane> {
        let f = self.tree.focus();
        self.pane_mut(f)
    }

    /// Tiled iff more than one pane and not zoomed. A single pane, or a zoomed
    /// pane, is passthrough.
    fn tiled(&self) -> bool {
        self.panes.len() > 1 && !self.zoomed
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `amux fleet …` is its own command family (a saved roster of agents), not a
    // hosted program — dispatch it before any of amux's flag parsing so `fleet`
    // and its subcommands are never mistaken for a command to host.
    if args.first().map(String::as_str) == Some("fleet") {
        return fleet_cmd(&args[1..]);
    }
    // `amux ctl …` is the control-channel client: it connects to the running
    // amux's `AMUX_CTL` endpoint, so it is a command family, not a hosted
    // program — dispatch it before flag parsing too.
    if args.first().map(String::as_str) == Some("ctl") {
        return amux::ctl::ctl_cmd(&args[1..]);
    }
    // amux's own `--identity <name>` / `-I <name>` is stripped off the front,
    // before the hosted command begins; it tags the initial agent pane and is
    // inherited by every split/new pane (stored, re-resolved per spawn). Only
    // the NAME is threaded through — never the resolved secret. We parse it
    // *first* so amux's own meta-flags (`--help`, `--stdin-probe`) are still
    // recognized when they follow an identity (`amux --identity work --help`).
    let (identity, rest) = amux::identity::parse(&args);
    if rest.first().map(String::as_str) == Some("--help") {
        eprintln!(
            "usage: amux [--identity <name>] [--allow-ctl [--max-depth <N>]] [--trust [plan|accept|automode|skip] | --skip-permissions] [-n <N> | --grid <R>x<C>] [command [args...]]\n\
             \x20      --trust <policy>: the session trust policy — the mode spawned agents run in, and the\n\
             \x20               ceiling they are capped at (low→high: plan < accept < automode < skip):\n\
             \x20               `plan` read-only plan mode; `accept` (bare --trust) auto-accept edits + a safe\n\
             \x20               dev-command allowlist (build/test/run), anything else (curl, git push, rm outside\n\
             \x20               the dir) still prompts, visibly; `automode` claude's auto mode (hands-off edits +\n\
             \x20               commands with claude's guardrails); `skip` FULL bypass (--dangerously-skip-\n\
             \x20               permissions, no gate — amux confirms it at launch). All pre-accept claude's\n\
             \x20               folder-trust dialog. Extend the accept allowlist with AMUX_TRUST_ALLOW=\"a,b\".\n\
             \x20               You (the root pane) can elevate a teammate above the policy per-spawn with\n\
             \x20               `ctl spawn --mode …`; a worker cannot.\n\
             \x20      --skip-permissions: alias for --trust skip.\n\
             \x20      amux ctl spawn [--role R] [--identity X] [--here] [--mode plan|accept|automode|skip] -- <cmd...> | list | send <target> <text> | status [target] | kill <target> | audit [N]\n\
             \x20      (AMUX_CTL_AUDIT=<file> mirrors the ctl audit log to JSONL)\n\
             \x20      (mouse capture is OFF by default so text selection works; Ctrl+A m turns it on: click focuses a pane, wheel scrolls the hovered tile)"
        );
        return ExitCode::SUCCESS;
    }
    if rest.first().map(String::as_str) == Some("--stdin-probe") {
        return stdin_probe();
    }
    // amux's own launch meta-flags (`--allow-ctl` / `--max-depth <N>` / `--trust`)
    // are stripped next — after `--identity`, before mass-spawn flags and the
    // hosted command. A bad `--max-depth` is a startup error, never a silent
    // fallback.
    let (allow_ctl, max_depth, trust, rest) = match amux::ctl::parse_flags(&rest) {
        Ok(quad) => quad,
        Err(msg) => {
            eprintln!("amux: {msg}");
            return ExitCode::FAILURE;
        }
    };
    // amux's own mass-spawn flags (`-n <N>` / `--grid <R>x<C>`) are stripped off
    // the front, after `--identity`, before the hosted command. A bad value is a
    // startup error, surfaced on stderr — never a silent fallback to one pane.
    let (grid, rest) = match amux::spawn::parse(&rest) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("amux: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let command: Vec<String> = if rest.is_empty() {
        vec![default_shell()]
    } else {
        rest
    };
    // `--skip-permissions` is full bypass — a conscious, dangerous choice. Make
    // the human confirm it once, in plain terms, *before* the TUI takes the
    // terminal (this reads stdin normally; the run loop takes raw mode after).
    if trust == amux::ctl::TrustMode::Skip && !confirm_skip_permissions() {
        eprintln!("amux: aborted (use --trust for safe hands-off: edits + a dev allowlist, dangerous commands still prompt).");
        return ExitCode::SUCCESS;
    }
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    run(
        &mut term,
        &command,
        identity.as_deref(),
        grid,
        None,
        allow_ctl,
        max_depth,
        trust,
    )
}

/// Plain-English confirmation gate for `--skip-permissions` (full bypass). Prints
/// what it does and the risk, then reads one line from stdin: only an explicit
/// `y`/`yes` proceeds. Any other input, EOF, or a non-interactive stdin aborts,
/// so the dangerous mode is never entered by accident. Runs before raw mode.
fn confirm_skip_permissions() -> bool {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "\n\x1b[1;31mWARNING: --skip-permissions runs EVERY command with no gate.\x1b[0m\n\
         Each agent pane (and every teammate it spawns) can run any command — delete\n\
         files, git push, network calls — with no approval prompt, on this machine with\n\
         your permissions. Use it only where damage is easily undone (a sandbox/VM).\n\
         For safe hands-off instead, quit and use --trust (edits + a dev allowlist;\n\
         dangerous commands still prompt, visibly)."
    );
    let _ = write!(err, "Proceed in full-bypass mode? [y/N] ");
    let _ = err.flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(n) if n > 0 => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        _ => false, // EOF / non-interactive → do not enter the dangerous mode
    }
}

// The run loop threads several independent, well-named launch parameters; a
// bag struct would obscure more than it helps here.
#[allow(clippy::too_many_arguments)]
fn run(
    term: &mut rawterm::Terminal,
    command: &[String],
    identity: Option<&str>,
    grid: Option<amux::spawn::Grid>,
    // A pre-built initial window (the fleet loader spawns its own panes, one per
    // agent, each with its own identity/cwd). When `Some`, it is used verbatim
    // as the first window and `command`/`grid` are ignored for it — but they
    // still drive later `Ctrl+A c` new panes and splits (which host `command`
    // under `identity`), so a fleet's new panes open a shell as a scratch pane.
    initial_window: Option<Window>,
    // ctl control plane (§ amux-ctl-control-plane): when `allow_ctl`, amux binds
    // a per-process control endpoint and injects its address into every pane, so
    // an agent inside a pane can `amux ctl spawn/list`. `max_depth` is the
    // recursion guard (usize::MAX == unlimited). Off ⇒ pre-ctl behavior verbatim.
    allow_ctl: bool,
    max_depth: usize,
    // How amux relaxes each spawned agent's permissions: `Off`, `Edits`
    // (`--trust`: acceptEdits + safe allowlist), or `Skip` (`--skip-permissions`:
    // full bypass). Published to the spawn path via `AGENT_TRUST` before the
    // first spawn.
    trust: amux::ctl::TrustMode,
) -> ExitCode {
    // Publish the trust policy before any pane is spawned so even the initial
    // agent picks it up.
    set_trust_mode(trust);
    let mut out = std::io::stdout();
    let (mut rows, mut cols) = term.size().unwrap_or((24, 80));
    // Alt screen; scroll region above the bar so bottom-line newlines from the
    // passthrough stream can never push the bar away.
    let _ = write!(out, "\x1b[?1049h\x1b[2J\x1b[H\x1b[1;{}r", rows - 1);
    let _ = out.flush();

    let mut windows: Vec<Window> = Vec::new();
    // The initial pane can fail to spawn (command missing) *or* fail to resolve
    // its identity (no such target, vault locked). A spawn failure aborts amux
    // (there is nothing to host); an identity-resolve failure is surfaced in the
    // bar and the pane spawns *without* the credential — never silently, and
    // never unauthenticated-without-saying-so (§7).
    let mut flash: Option<(String, Instant)> = None;
    // ctl control channel (opt-in). Bind the endpoint and publish its address to
    // the spawn path *before* the first pane is spawned, so every pane — the
    // initial one included — is born with `AMUX_CTL`/`AMUX_PANE` in its
    // environment. A bind failure is non-fatal: amux still runs, just without
    // ctl, and says so in the bar (never silently unavailable).
    let mut ctl_listener: Option<amux::ipc::Listener> = None;
    // Queue-until-idle deliveries for `ctl send` (flushed each tick).
    let mut pending_sends: Vec<PendingSend> = Vec::new();
    // Operator-approved extra allowlist stems (AMUX_CTL_ALLOW); empty ⇒ the
    // built-in agents-only guard. Read once at startup.
    let ctl_extra_allow = if allow_ctl {
        amux::ctl::extra_allow_from_env()
    } else {
        Vec::new()
    };
    if allow_ctl {
        let addr = amux::ipc::default_address();
        match amux::ipc::Listener::bind(&addr) {
            Ok(l) => {
                let _ = CTL_ADDRESS.set(addr);
                ctl_listener = Some(l);
            }
            Err(e) => {
                flash = Some((format!("ctl channel disabled: {e}"), Instant::now()));
            }
        }
    }
    // ctl audit log (design §5): in-memory always when ctl is on, mirrored to a
    // JSONL file when the operator opts in via `AMUX_CTL_AUDIT`. A file that
    // can't be opened is surfaced once in the bar; the in-memory log runs on.
    let mut ctl_audit = if allow_ctl {
        let path = std::env::var(amux::ctl::ENV_AUDIT)
            .ok()
            .filter(|p| !p.is_empty());
        let mut a = amux::audit::Audit::new(amux::audit::DEFAULT_CAP, path.as_deref());
        if let Some(err) = a.take_error() {
            if flash.is_none() {
                flash = Some((format!("ctl audit: {err}"), Instant::now()));
            }
        }
        a
    } else {
        amux::audit::Audit::in_memory()
    };
    // A pre-built window (fleet) is used as-is; otherwise mass-spawn opens one
    // window of N tiles in a balanced grid, or the 0.1 single-pane path. All
    // share the same spawn machinery (each pane its own session).
    let prebuilt = initial_window.is_some();
    let initial = match initial_window {
        Some(w) => Ok(w),
        None => match grid {
            Some(g) => spawn_window_grid(command, rows, cols, g, identity, &mut flash),
            None => spawn_window(command, rows, cols, 0, identity, trust_mode(), &mut flash),
        },
    };
    match initial {
        Ok(w) => windows.push(w),
        Err(e) => {
            cleanup_screen(&mut out);
            eprintln!("amux: cannot start {:?}: {e}", command[0]);
            return ExitCode::FAILURE;
        }
    }
    // A grid or pre-built (fleet) window is born tiled: resize so every pane's
    // pty/emulator gets its true inner rect (spawn used rough sizes).
    if grid.is_some() || prebuilt {
        resize_window(&mut windows[0], rows, cols);
    }
    let mut active = 0usize; // active window index
    let mut scanner = PrefixScanner::new();
    // Mouse capture is OFF by default so the terminal's own **text selection**
    // works out of the box (dragging highlights, as in any shell) — the common
    // case. `Ctrl+A m` toggles capture ON when you want the amux mouse: click to
    // focus the pane under the cursor, wheel to scroll the hovered tile. (With
    // capture on, native selection falls back to Shift-drag.) The scanner and the
    // terminal are kept in sync by the toggle.
    let mut mouse_on = false;
    // Whether amux has forced the OUTER terminal's mouse reporting off because the
    // focused pane does not want the mouse (a shell/WSL). A mouse app (claude)
    // enables motion tracking via passthrough, which leaks to the terminal; once
    // you switch to a non-mouse pane the terminal keeps sending motion events and
    // they get forwarded into that pane as garbage text. See the re-assert below.
    let mut outer_mouse_off = false;
    // The full-screen board dashboard overlay (`Ctrl+A b`). While on, the panes
    // keep running (drained, emulated) but are not painted, and keystrokes don't
    // reach them — it's a read-only view of the shared board.
    let mut board_view = false;
    // The command prompt (`Ctrl+A :`): `Some(line)` while the operator is typing a
    // command to open in a new pane (any shell/program, not just the launch one).
    // Keystrokes edit the line instead of reaching the panes; Enter opens it.
    let mut prompt: Option<String> = None;
    let mut buf = [0u8; 8192];
    let mut force_repaint = true;
    let mut last_bar_paint = Instant::now();
    let mut last_size_check = Instant::now();
    // Animation clock for the per-pane loading spinner (advances ~8 frames/sec).
    // A blank (still-booting) pane's spinner changes with this, so the tiled diff
    // repaints just those cells; once every pane has painted, the frame no longer
    // affects the master, so it stops driving repaints.
    let anim_start = Instant::now();
    // The spinner frame last painted by the passthrough startup splash, so it
    // redraws only when the frame advances (not every tick).
    let mut last_splash_frame: usize = usize::MAX;
    let mut last_bar = String::new();

    // Agent session state (§5): one read-only `agsess::World` over the Claude
    // projects root, polled on the loop's existing `Instant`-throttle pattern —
    // no threads. The *first* refresh uses `refresh_since(process_start_ms)` so
    // the cold history scan (agtop measures ~1.4 s for ~60 MB) never freezes
    // keystrokes; amux can never care about a session that stopped writing
    // before it started. Thereafter: bound panes tail ~1 s, discovery ~5 s
    // (accelerated to ~1 s while any agent pane is still unbound).
    // The shared board (coordination layer): source-of-truth team state, in-memory
    // unless AMUX_BOARD names a snapshot file. Part of the ctl surface, so it is
    // already gated by `--allow-ctl`.
    let mut board = match std::env::var_os(amux::board::ENV_BOARD) {
        Some(p) if !p.is_empty() => amux::board::Board::with_file(std::path::PathBuf::from(p)),
        _ => amux::board::Board::new(),
    };
    // The shared pub/sub bus (coordination layer part 2): the team's event stream,
    // in-memory unless AMUX_BUS names a snapshot file. Same ctl gating as the board.
    let mut bus = match std::env::var_os(amux::bus::ENV_BUS) {
        Some(p) if !p.is_empty() => amux::bus::Bus::with_file(std::path::PathBuf::from(p)),
        _ => amux::bus::Bus::new(),
    };
    let mut world = agsess::World::new(agsess::default_root());
    let process_start_ms = agsess::sessions::now_ms();
    world.refresh_since(process_start_ms);
    let mut last_agent_poll = Instant::now();
    let mut last_agent_discover = Instant::now();
    // The previous composited master, kept per-frame so tiled mode diffs. Reset
    // to None (full repaint) on mode/layout/window changes.
    let mut prev_master: Option<ansi::Screen> = None;

    // The read at the top can fail (terminal gone) *and* commands deep inside
    // `break 'outer`; a labeled `loop` expresses both. clippy's while-let
    // rewrite can't host the labeled break, so allow it here.
    #[allow(clippy::while_let_loop)]
    'outer: loop {
        // 1. keystrokes -> scanner -> focused pane / commands
        let bytes = match term.read_bytes(Duration::from_millis(15)) {
            Ok(b) => b,
            Err(_) => break,
        };
        if !bytes.is_empty() && std::env::var_os("AMUX_DEBUG").is_some() {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprint!("[amux-dbg stdin {}]\r\n", hex.join(" "));
        }
        // The command prompt captures keystrokes: while it is open, keys edit the
        // command line (not the panes), and the scanner is fed nothing. Enter opens
        // the typed command in a new pane; Esc/Ctrl+C cancels.
        let feed: &[u8] = if prompt.is_some() {
            match edit_prompt(prompt.as_mut().unwrap(), &bytes) {
                PromptEdit::Continue => {}
                PromptEdit::Cancel => prompt = None,
                PromptEdit::Submit => {
                    let argv = split_cmdline(prompt.take().unwrap_or_default().trim());
                    if !argv.is_empty() {
                        match spawn_window(
                            &argv,
                            rows,
                            cols,
                            windows.len(),
                            identity,
                            trust_mode(),
                            &mut flash,
                        ) {
                            Ok(w) => {
                                windows.push(w);
                                let last = windows.len() - 1;
                                switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                                prev_master = None;
                            }
                            Err(e) => {
                                flash = Some((
                                    format!("cannot start {:?}: {e}", argv[0]),
                                    Instant::now(),
                                ));
                            }
                        }
                    }
                }
            }
            // Repaint only when the user actually pressed something (the line
            // changed, or it opened/closed) — NOT every idle tick, which made the
            // prompt row flash rapidly.
            if !bytes.is_empty() {
                force_repaint = true;
            }
            b""
        } else {
            &bytes
        };
        for action in scanner.feed(feed) {
            match action {
                Action::Forward(b) => {
                    // While the board overlay is up, swallow input — the panes are
                    // hidden, so keystrokes shouldn't reach them.
                    if !board_view {
                        if let Some(p) = windows[active].focused_mut() {
                            let _ = p.pty.write(&b);
                        }
                    }
                }
                Action::NextPane => {
                    let next = (active + 1) % windows.len();
                    switch_window(&mut windows, &mut active, next, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::PrevPane => {
                    let prev = (active + windows.len() - 1) % windows.len();
                    switch_window(&mut windows, &mut active, prev, rows, cols, &mut out);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SwitchTo(i) => {
                    if i < windows.len() {
                        switch_window(&mut windows, &mut active, i, rows, cols, &mut out);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::NewPane => {
                    match spawn_window(command, rows, cols, windows.len(), identity, trust_mode(), &mut flash) {
                        Ok(w) => {
                            windows.push(w);
                            let last = windows.len() - 1;
                            switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                            prev_master = None;
                            force_repaint = true;
                        }
                        Err(e) => {
                            flash = Some((
                                format!("cannot start {:?}: {e}", command[0]),
                                Instant::now(),
                            ));
                            force_repaint = true;
                        }
                    }
                }
                Action::NewShellPane => {
                    // A plain shell in a new window — no identity, no trust posture
                    // (it's not an agent), but it still gets the ctl env injected,
                    // so you can run `amux ctl board list` here and see it rendered.
                    let shell = vec![default_shell()];
                    match spawn_window(
                        &shell,
                        rows,
                        cols,
                        windows.len(),
                        None,
                        amux::ctl::TrustMode::Off,
                        &mut flash,
                    ) {
                        Ok(w) => {
                            windows.push(w);
                            let last = windows.len() - 1;
                            switch_window(&mut windows, &mut active, last, rows, cols, &mut out);
                            prev_master = None;
                            force_repaint = true;
                        }
                        Err(e) => {
                            flash = Some((
                                format!("cannot start shell {:?}: {e}", shell[0]),
                                Instant::now(),
                            ));
                            force_repaint = true;
                        }
                    }
                }
                Action::ToggleBoard => {
                    board_view = !board_view;
                    // Toggling either way is a full repaint: on → draw the panel
                    // (it clears the screen); off → recompose/repaint the panes the
                    // panel covered. On close, restore the cursor the panel hid.
                    if !board_view {
                        let _ = write!(out, "\x1b[?25h");
                    }
                    prev_master = None;
                    force_repaint = true;
                }
                Action::OpenPrompt => {
                    // Start the command prompt; subsequent keystrokes edit the line
                    // (handled above the scanner) until Enter/Esc.
                    prompt = Some(String::new());
                    force_repaint = true;
                }
                Action::SplitH => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Horizontal,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::SplitV => {
                    split_focused(
                        &mut windows[active],
                        layout::Dir::Vertical,
                        command,
                        rows,
                        cols,
                        identity,
                        &mut flash,
                    );
                    resize_window(&mut windows[active], rows, cols);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::MoveFocus(d) => {
                    let outer = tiled_outer(rows, cols);
                    windows[active].tree.move_focus(to_move(d), outer);
                    prev_master = None;
                    force_repaint = true;
                }
                Action::Zoom => {
                    let w = &mut windows[active];
                    // Zoom only means something with more than one pane.
                    if w.panes.len() > 1 {
                        w.zoomed = !w.zoomed;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    }
                }
                Action::ToggleMouse => {
                    mouse_on = !mouse_on;
                    // Keep the terminal's capture and the scanner's parsing in
                    // lockstep — set both, and don't leave one on if the other
                    // fails.
                    if term.set_mouse(mouse_on).is_ok() {
                        scanner.set_mouse(mouse_on);
                        flash = Some((
                            if mouse_on {
                                "mouse: ON — click focuses, wheel scrolls the hovered tile (Shift-drag selects)"
                                    .to_string()
                            } else {
                                "mouse: OFF — native drag to select / copy".to_string()
                            },
                            Instant::now(),
                        ));
                    } else {
                        mouse_on = !mouse_on; // revert on failure
                        flash = Some(("mouse mode unavailable here".to_string(), Instant::now()));
                    }
                    force_repaint = true;
                }
                Action::MouseClick { col, row } => {
                    // Focus the pane under the cursor. Only meaningful in a tiled
                    // window (a single/zoomed pane already has focus).
                    let w = &mut windows[active];
                    if w.tiled() {
                        let outer = tiled_outer(rows, cols);
                        let mx = col.saturating_sub(1) as usize;
                        let my = row.saturating_sub(1) as usize;
                        for (id, rect) in w.tree.rects(outer) {
                            let inside = my >= rect.row
                                && my < rect.row + rect.rows
                                && mx >= rect.col
                                && mx < rect.col + rect.cols;
                            if inside {
                                if w.tree.focus_pane(id) {
                                    prev_master = None;
                                    force_repaint = true;
                                }
                                break;
                            }
                        }
                    }
                }
                Action::MouseScroll { up, col, row } => {
                    // Route a wheel notch to the tile under the cursor (not
                    // necessarily the focused one) so you scroll whatever you're
                    // hovering. Forward a translated SGR wheel event to that pane's
                    // app — only if it wants the mouse, so a bare shell never gets
                    // stray bytes. `64` = wheel up, `65` = wheel down.
                    let w = &mut windows[active];
                    let notch = if up { 64 } else { 65 };
                    if w.tiled() {
                        let outer = tiled_outer(rows, cols);
                        let mx = col.saturating_sub(1) as usize;
                        let my = row.saturating_sub(1) as usize;
                        let hit = w.tree.rects(outer).into_iter().find(|(_, r)| {
                            my >= r.row
                                && my < r.row + r.rows
                                && mx >= r.col
                                && mx < r.col + r.cols
                        });
                        if let Some((id, rect)) = hit {
                            if let Some(p) = w.pane_mut(id) {
                                if p.mouse_wanted {
                                    // Master cell → the pane's inner (bordered)
                                    // 1-based coords: content is inset one cell.
                                    let inner_cols = rect.cols.saturating_sub(2).max(1);
                                    let inner_rows = rect.rows.saturating_sub(2).max(1);
                                    let cx = mx
                                        .saturating_sub(rect.col + 1)
                                        .min(inner_cols - 1)
                                        + 1;
                                    let cy = my
                                        .saturating_sub(rect.row + 1)
                                        .min(inner_rows - 1)
                                        + 1;
                                    let seq = format!("\x1b[<{notch};{cx};{cy}M");
                                    let _ = p.pty.write(seq.as_bytes());
                                }
                            }
                        }
                    } else if let Some(p) = windows[active].focused_mut() {
                        // Passthrough / zoom: the sole pane fills the area above the
                        // bar; forward with the original coordinates.
                        if p.mouse_wanted {
                            let seq = format!("\x1b[<{notch};{col};{row}M");
                            let _ = p.pty.write(seq.as_bytes());
                        }
                    }
                }
                Action::KillPane => {
                    let w = &mut windows[active];
                    let victim = w.tree.focus();
                    if w.panes.len() > 1 {
                        // Multi-pane window: close synchronously so focus leaves
                        // the dying pane at once (the next keystrokes must route
                        // to the survivor, not the corpse) and the frame re-tiles
                        // without waiting for the async reap.
                        if let Some(p) = w.pane_mut(victim) {
                            let _ = p.pty.kill();
                        }
                        w.tree.close(victim);
                        w.panes.retain(|p| p.id != victim);
                        w.zoomed = w.zoomed && w.panes.len() > 1;
                        resize_window(w, rows, cols);
                        prev_master = None;
                        force_repaint = true;
                    } else if let Some(p) = w.pane_mut(victim) {
                        // Sole pane: async kill; the reap step drops the window
                        // and the app exits when the last window is gone (0.1).
                        let _ = p.pty.kill();
                    }
                }
                Action::Quit => {
                    if std::env::var_os("AMUX_DEBUG").is_some() {
                        eprint!("[amux-dbg quit-received]\r\n");
                    }
                    break 'outer;
                }
            }
        }

        // 1b. ctl control channel: drain up to a few requests this tick
        //     (non-blocking; usually zero). Each request is applied as a pane
        //     operation and answered on the same connection. A spawn appends a
        //     visible new window, so we force a repaint after any request.
        if let Some(listener) = ctl_listener.as_mut() {
            for _ in 0..8 {
                match listener.poll() {
                    Ok(Some(line)) => {
                        let reply = apply_ctl(
                            &line,
                            &mut windows,
                            rows,
                            cols,
                            max_depth,
                            &ctl_extra_allow,
                            &mut pending_sends,
                            &world,
                            identity,
                            &mut ctl_audit,
                            &mut board,
                            &mut bus,
                        );
                        let _ = listener.respond(&reply);
                        prev_master = None;
                        force_repaint = true;
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }

        // 1c. flush any queued `ctl send`s whose target is now idle (Decision 4).
        if flush_sends(&mut pending_sends, &mut windows, &world) {
            force_repaint = true;
        }

        // 2. drain every pane in the active window (all panes are live in
        //    tiled mode); feed the emulator, and in passthrough also write the
        //    focused pane's cleaned bytes straight through.
        let tiled = windows[active].tiled();
        let focus = windows[active].tree.focus();
        for pane in windows[active].panes.iter_mut() {
            // Drain each active pane *fully* before we composite, so a big data
            // burst (a large `ctl send`, a wall of tool output) is a whole frame
            // rather than a truncated one — a partial drain that straddles a
            // `?2026` block stalls the outer terminal (the "shutter"). The loop
            // still breaks the instant there is no more data, so the high cap only
            // bites on a genuinely huge burst; it never adds latency when idle.
            for i in 0..DRAIN_READS_PER_TICK {
                let wait = if i == 0 {
                    Duration::from_millis(5)
                } else {
                    Duration::ZERO
                };
                match pane.pty.read_timeout(&mut buf, wait) {
                    Ok(Some(n)) if n > 0 => {
                        // Always feed the emulator so a later switch/split/zoom
                        // renders the current screen without a repaint nudge.
                        pane.term.feed(&buf[..n]);
                        // Track whether this pane's app wants the mouse (so scroll
                        // routes to it, not a bare shell).
                        if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                            pane.mouse_wanted = m;
                        }
                        // "painted" = the emulator now has *visible* content, not
                        // merely that setup bytes arrived — so the splash stays up
                        // until the agent's first frame.
                        let first_paint = !pane.painted && !term_blank(&pane.term);
                        if first_paint {
                            pane.painted = true;
                        }
                        if !tiled && pane.id == focus {
                            if board_view {
                                // Board overlay is up: keep the passthrough filter
                                // state current, but don't paint the pane over the
                                // panel. (The emulator was already fed above, so a
                                // toggle-off repaints from the current screen.)
                                let _ = pane.filter.feed(&buf[..n]);
                            } else if first_paint {
                                // Hand off from the splash: restore the cursor the
                                // splash hid, then paint the agent's current screen
                                // straight from the emulator (its `render_full`
                                // already clears + reflects everything fed so far),
                                // and resume live passthrough from here.
                                let full = pane.term.screen().render_full();
                                let _ = out.write_all(b"\x1b[?25h");
                                let _ = out.write_all(&full);
                                // render_full cleared the whole screen (the bar row
                                // too); repaint the bar this tick so it never blinks
                                // out during the handoff.
                                force_repaint = true;
                            } else if pane.painted {
                                let cleaned = pane.filter.feed(&buf[..n]);
                                let _ = out.write_all(&cleaned);
                                // A full-screen clear from the app (claude emits one
                                // at startup, and again as its UI settles) ignores
                                // the scroll region and wipes the bar row. Repaint the
                                // bar this tick so it doesn't vanish until some later
                                // trigger (which is why it only appeared once ctl/
                                // remote-control connected).
                                if clears_screen(&buf[..n]) {
                                    force_repaint = true;
                                }
                            } else {
                                // Still on the splash: keep the passthrough filter's
                                // state current, but suppress output so the agent's
                                // setup bytes don't scribble under the splash.
                                let _ = pane.filter.feed(&buf[..n]);
                            }
                        } else if pane.id != focus {
                            pane.activity = true;
                            pane.painted = true; // background/tiled: any output = painted
                        }
                    }
                    Ok(Some(_)) => {
                        pane.exited = true;
                        break;
                    }
                    _ => break,
                }
            }
        }
        if !tiled {
            let _ = out.flush();
        }

        // 3. background windows: drain (discarded — emulator/ConPTY keep the
        //    screen) and flag activity so the bar shows it.
        for (i, w) in windows.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            for pane in w.panes.iter_mut() {
                while let Ok(Some(n)) = pane.pty.read_timeout(&mut buf, Duration::ZERO) {
                    if n == 0 {
                        pane.exited = true;
                        break;
                    }
                    pane.term.feed(&buf[..n]);
                    if let Some(m) = sniff_mouse_mode(&buf[..n]) {
                        pane.mouse_wanted = m;
                    }
                    pane.activity = true;
                }
            }
        }

        // 4. reap exits; drop dead panes and empty windows, re-tiling.
        let mut layout_changed = false;
        for w in windows.iter_mut() {
            for pane in w.panes.iter_mut() {
                if let Ok(Some(_)) = pane.pty.try_wait() {
                    pane.exited = true;
                }
            }
            if w.panes.iter().any(|p| p.exited) {
                let dead: Vec<usize> = w.panes.iter().filter(|p| p.exited).map(|p| p.id).collect();
                for id in dead {
                    // Collapse the tree first (keeps focus valid), then drop the
                    // pane. A last-pane close leaves the tree single; the window
                    // itself is removed below when its panes go empty.
                    let _ = w.tree.close(id);
                }
                w.panes.retain(|p| !p.exited);
                w.zoomed = w.zoomed && w.panes.len() > 1;
                layout_changed = true;
            }
        }
        if layout_changed {
            let empty_before_active = windows[..active]
                .iter()
                .filter(|w| w.panes.is_empty())
                .count();
            windows.retain(|w| !w.panes.is_empty());
            if windows.is_empty() {
                break;
            }
            active = active
                .saturating_sub(empty_before_active)
                .min(windows.len() - 1);
            resize_window(&mut windows[active], rows, cols);
            prev_master = None;
            force_repaint = true;
        }

        // 4b. Match the OUTER terminal's mouse reporting to the focused pane. A
        //     mouse app (claude) turns on motion tracking, which passes through to
        //     the terminal; when you then focus a pane that does NOT want the mouse
        //     (a shell, WSL), the terminal keeps sending motion events and they get
        //     forwarded into that pane as literal text (`35;79;16M…`). Force mouse
        //     reporting OFF for a non-mouse focused pane; a mouse app re-asserts its
        //     own modes on its next repaint when you switch back. Skipped while the
        //     global mouse capture (`Ctrl+A m`) is on.
        let focus_wants_mouse = {
            let f = windows[active].tree.focus();
            windows[active].pane(f).map(|p| p.mouse_wanted).unwrap_or(false)
        };
        if !mouse_on && !focus_wants_mouse {
            if !outer_mouse_off {
                let _ = out.write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l");
                let _ = out.flush();
                outer_mouse_off = true;
            }
        } else {
            outer_mouse_off = false;
        }

        // 5. resize propagation
        if last_size_check.elapsed() >= Duration::from_millis(150) {
            last_size_check = Instant::now();
            if let Ok((r, c)) = term.size() {
                // Windows Terminal's ConPTY reports a few rows FEWER once amux is
                // in the alternate screen buffer (a persistent reservation), and
                // adopting it makes amux redraw short — leaving a strip of stale
                // content below the bar (the initial full-screen draw was correct).
                // Treat a small height-only shrink as that reservation and keep the
                // current size; real resizes (width change, growth, or a large
                // shrink) still apply.
                let altscreen_reserve =
                    c == cols && r < rows && rows - r <= ALT_SCREEN_RESERVE_ROWS;
                if (r, c) != (rows, cols) && r >= 3 && !altscreen_reserve {
                    rows = r;
                    cols = c;
                    // Reset the scroll region for the new height, then wipe the
                    // whole screen. The wipe is what fixes the tiled-resize
                    // garble: when the terminal shrinks, cells from the previous,
                    // larger frame lie outside the new master and would otherwise
                    // linger; and a tiled recompose diffs against `prev_master`,
                    // so without this clear + `prev_master = None` the first frame
                    // at the new size can paint over stale geometry. This mirrors
                    // the clear `switch_window`/`repaint_focused` already emit.
                    let _ = write!(out, "\x1b[1;{}r\x1b[2J\x1b[H", rows - 1);
                    // Resize every window's panes (pty + emulator) to their new
                    // inner rects at the new size — tiled windows recompute the
                    // grid, passthrough windows refit the sole/focused pane.
                    // `resize_window` is the single helper both initial layout
                    // and resize share, so their inner-rect math cannot drift.
                    for w in windows.iter_mut() {
                        resize_window(w, rows, cols);
                    }
                    // Force a full redraw of the focused passthrough pane at the new
                    // size with a *second* resize (to size-1, then size). A single
                    // resize can be missed by an app mid-boot or mid-reconnect —
                    // claude keeps drawing at the stale size, ending up in a corner
                    // of the larger terminal. The extra resize guarantees a fresh
                    // SIGWINCH/redraw. (The `force_repaint` path below only nudges
                    // once the pane has painted, so it misses exactly the boot
                    // window the founder hit; this covers it.)
                    if !windows[active].tiled() {
                        let ar = rows.saturating_sub(1).max(1);
                        let focus = windows[active].tree.focus();
                        if let Some(p) = windows[active].pane_mut(focus) {
                            let _ = p.pty.resize(ar.saturating_sub(1).max(1), cols);
                            let _ = p.pty.resize(ar, cols);
                            p.term.resize(ar as usize, cols as usize);
                        }
                    }
                    // Force a full recompose at the new size: tiled repaints via
                    // `render_full` (which re-clears + paints every cell),
                    // passthrough nudges the focused pty to redraw.
                    prev_master = None;
                    force_repaint = true;
                }
            }
        }

        // 5b. agent state (§5). `refresh` tails only files that grew, so the
        //     bound-pane tick is cheap; discovery is the full `read_dir`, run
        //     rarely — but accelerated while any agent pane still lacks its
        //     transcript so a fresh agent binds promptly.
        let any_unbound = windows.iter().any(|w| {
            w.panes.iter().any(|p| {
                p.session_id
                    .as_deref()
                    .is_some_and(|id| !world.sessions.iter().any(|s| s.id == id))
            })
        });
        let discover_every = if any_unbound {
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(5000)
        };
        // While the sole pane is still on the startup splash, skip the refresh
        // entirely: nothing is bound yet (so the bar has nothing to show), and the
        // discovery pass is a full `read_dir` over the projects root that can block
        // the loop for the better part of a second — which froze the splash spinner
        // for a beat right at ~1 s in. Once the agent paints, polling resumes and
        // it binds on the next tick.
        let booting_splash = !windows[active].tiled()
            && windows[active]
                .pane(windows[active].tree.focus())
                .map(|p| !p.painted)
                .unwrap_or(false);
        if booting_splash {
            // hold off — resumes as soon as the pane paints
        } else if last_agent_discover.elapsed() >= discover_every {
            // Discovery + tail: a full refresh picks up new transcripts and
            // tails grown ones in one pass.
            world.refresh();
            last_agent_discover = Instant::now();
            last_agent_poll = Instant::now();
        } else if last_agent_poll.elapsed() >= Duration::from_millis(1000) {
            // Bound-pane tick: refresh tails only files whose length grew, so
            // this is ~a handful of stats for a small grid.
            world.refresh();
            last_agent_poll = Instant::now();
        }

        // 6. render the active window (tiled compose+diff, else passthrough is
        //    already written above) then paint the bar. The tiled composite and
        //    the bar are accumulated into one `frame` and emitted wrapped in
        //    synchronized output (§`SYNC_BEGIN`), so the outer terminal paints
        //    panes+bar atomically — no mid-frame tearing. An empty frame (idle
        //    tick) emits nothing, so the markers never spam.
        let spin_frame = (anim_start.elapsed().as_millis() / 120) as usize;
        let mut frame: Vec<u8> = Vec::new();
        // Set when this tick composited the startup splash into `frame`; its `2J`
        // wipes the bar, so the bar is force-appended below to keep the frame whole.
        let mut splash_drawn = false;
        if board_view {
            // The board overlay replaces the panes. Re-render only on a repaint
            // (toggle, or a board write — every ctl request forces one), so idle
            // ticks leave the panel steady; the bar still paints below.
            if force_repaint {
                frame.extend_from_slice(render_board_panel(&board, &bus, rows, cols).as_bytes());
            }
        } else if windows[active].tiled() {
            let master = render_tiled(&windows[active], rows, cols, &world.sessions, spin_frame);
            match &prev_master {
                Some(prev) => frame.extend_from_slice(&prev.diff(&master)),
                None => frame.extend_from_slice(&master.render_full()),
            };
            prev_master = Some(master);
        } else {
            // Passthrough (single pane or zoomed). While the focused pane has not
            // painted yet, animate the startup splash so the ~seconds of agent
            // boot don't look like a hang; the drain wipes it and takes over on
            // the pane's first bytes. Throttled to the spinner's ~8 fps.
            let fp = windows[active].tree.focus();
            let unpainted = windows[active].pane(fp).map(|p| !p.painted).unwrap_or(false);
            if unpainted {
                if spin_frame != last_splash_frame {
                    draw_startup_splash(&mut frame, rows, cols, spin_frame);
                    last_splash_frame = spin_frame;
                    splash_drawn = true;
                }
            } else if force_repaint {
                // Nudge the focused pane's pty to repaint in full, the same trick
                // 0.1 uses on window switch.
                repaint_focused(&mut windows[active], rows, cols, &mut out);
            }
        }

        // 7. the bar (windows, with the active one starred)
        let infos: Vec<PaneInfo> = windows
            .iter()
            .enumerate()
            .map(|(i, w)| PaneInfo {
                title: w.panes.first().map(|p| p.title.clone()).unwrap_or_default(),
                active: i == active,
                activity: w.panes.iter().any(|p| p.activity),
                exited: w.panes.iter().all(|p| p.exited),
                // A window is "waiting" when a bound agent pane in it is blocked
                // on the human (§4.2 rung 3). Status only.
                waiting: w.panes.iter().any(|p| {
                    matches!(
                        amux::bind::status_for(p.session_id.as_deref(), &world.sessions),
                        Some(agsess::Status::WaitingApproval)
                    )
                }),
                // The window's identity tag: the first pane's identity name (all
                // panes in a window inherit the same identity in v1). Name only.
                identity: w.panes.first().and_then(|p| p.identity.clone()),
            })
            .collect();
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            flash = None;
            force_repaint = true;
        }
        // A flash note wins; otherwise, if the bus has open `decision_needed`
        // escalations, surface them on the bar so you see them without opening the
        // `Ctrl+A b` panel — the bus's push channel to the human.
        let decisions_open = bus.pending_decisions().len();
        let decision_note = if decisions_open > 0 {
            format!(
                "{decisions_open} decision{} need you \u{00b7} Ctrl+A b",
                if decisions_open == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let note = flash.as_ref().map(|(m, _)| m.as_str()).unwrap_or({
            if decision_note.is_empty() {
                ""
            } else {
                decision_note.as_str()
            }
        });
        // While the command prompt is open it takes over the bar row: a `:` and
        // the line typed so far, cursor left right after it (visible) so you can
        // see what you're launching. Otherwise the normal status bar.
        let painted = if let Some(line) = &prompt {
            format!("\x1b[{};1H\x1b[2K\x1b[1;38;2;70;235;255m:\x1b[0m{line}\x1b[?25h", rows)
        } else {
            bar_paint(&infos, rows, cols as usize, note)
        };
        let mut bar_appended = false;
        if force_repaint
            || splash_drawn
            || painted != last_bar
            // The 500ms periodic refresh keeps the status bar's activity markers
            // live, but while the command prompt is open it would reflash the
            // prompt row twice a second — skip it there (the prompt repaints on
            // keystroke via `painted != last_bar`).
            || (prompt.is_none() && last_bar_paint.elapsed() >= Duration::from_millis(500))
        {
            frame.extend_from_slice(painted.as_bytes());
            last_bar = painted;
            last_bar_paint = Instant::now();
            bar_appended = true;
        }
        // The bar leaves the cursor parked on the bar row. Move it back to the
        // pane's real cursor with an explicit CUP — NOT DECSC/DECRC, whose single
        // save slot is shared with the hosted app (claude parks its cursor there
        // for its own menus; saving/restoring around the bar corrupted it and left
        // redraw fragments in passthrough). We track the cursor ourselves: the
        // tiled master carries it, and even a passthrough pane is fed to its
        // emulator, so `term.screen().cursor` is current. Both are 0-based; the
        // passthrough pane fills the screen from (0,0), so +1 gives 1-based screen
        // coordinates in either mode.
        if bar_appended && !board_view && prompt.is_none() {
            let cur = if windows[active].tiled() {
                prev_master.as_ref().map(|m| m.cursor)
            } else {
                let fp = windows[active].tree.focus();
                windows[active].pane(fp).map(|p| p.term.screen().cursor)
            };
            if let Some((cr, cc)) = cur {
                frame.extend_from_slice(format!("\x1b[{};{}H", cr + 1, cc + 1).as_bytes());
            }
        }
        // Emit the tick's composite+bar as ONE synchronized frame, so the outer
        // terminal never shows it half-drawn (the tiled "shutter"). Nothing to
        // draw ⇒ no write, no markers.
        if !frame.is_empty() {
            let _ = out.write_all(SYNC_BEGIN);
            let _ = out.write_all(&frame);
            let _ = out.write_all(SYNC_END);
            let _ = out.flush();
        }
        force_repaint = false;
    }

    let dbg = std::env::var_os("AMUX_DEBUG").is_some();
    for w in windows.iter_mut() {
        for pane in w.panes.iter_mut() {
            let _ = pane.pty.kill();
        }
    }
    if dbg {
        eprint!("[amux-dbg killed]\r\n");
    }
    cleanup_screen(&mut out);
    if dbg {
        eprint!("[amux-dbg cleaned]\r\n");
    }
    drop(windows);
    if dbg {
        eprint!("[amux-dbg panes-dropped]\r\n");
    }
    ExitCode::SUCCESS
}

/// The tiled drawing area: the whole terminal minus the bar row (last line).
fn tiled_outer(rows: u16, cols: u16) -> Rect {
    Rect {
        row: 0,
        col: 0,
        rows: (rows as usize).saturating_sub(1).max(1),
        cols: cols as usize,
    }
}

fn to_move(d: Dir) -> layout::Move {
    match d {
        Dir::Left => layout::Move::Left,
        Dir::Right => layout::Move::Right,
        Dir::Up => layout::Move::Up,
        Dir::Down => layout::Move::Down,
    }
}

/// Compose the active window's panes into a master screen (boxed borders +
/// titles + liveness color + content); the caller diffs it to the terminal.
fn render_tiled(
    w: &Window,
    rows: u16,
    cols: u16,
    sessions: &[agsess::AgentSession],
    frame: usize,
) -> ansi::Screen {
    let outer = tiled_outer(rows, cols);
    let rects = w.tree.rects(outer);
    let focus = w.tree.focus();
    let views: Vec<PaneView> = rects
        .iter()
        .filter_map(|(id, rect)| {
            w.pane(*id).map(|p| {
                // Liveness in priority order: focus, then a dead child, then
                // recent background activity, then plain idle. `index` is the
                // pane's 1-based position in the split's leaf order.
                let state = if *id == focus {
                    PaneState::Focused
                } else if p.exited {
                    PaneState::Exited
                } else if p.activity {
                    PaneState::Active
                } else {
                    PaneState::Idle
                };
                // Agent mark only for *unfocused* bound agent panes (§4.1): a
                // blocked agent in the focused pane needs no escalation. The
                // binder maps the pane's injected id to its live status; carries
                // status only — never any transcript text.
                let agent = if *id == focus {
                    None
                } else {
                    amux::bind::status_for(p.session_id.as_deref(), sessions)
                        .map(|status| AgentMark { status })
                };
                let index = w.panes.iter().position(|q| q.id == *id).unwrap_or(0) + 1;
                PaneView {
                    screen: p.term.screen(),
                    rect: *rect,
                    index,
                    title: &p.title,
                    state,
                    agent,
                    identity: p.identity.as_deref(),
                }
            })
        })
        .collect();
    // Compose over the full terminal (rows), leaving the bar row untouched; the
    // bar is painted separately after the diff, exactly as in passthrough.
    compose(rows as usize, cols as usize, &views, frame)
}

/// Split the focused pane, spawning a new pane sized to what its half will be.
/// On spawn failure the split is abandoned and the reason lands in the bar.
fn split_focused(
    w: &mut Window,
    dir: layout::Dir,
    command: &[String],
    rows: u16,
    cols: u16,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) {
    let new_id = w.next_id;
    // Size the new pane roughly to a half's *inner* area (minus the border);
    // resize_window fixes it exactly right after the split.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    // Splits inherit the active identity (§4). resolve failure lands in `flash`.
    match spawn_pane(command, pr.max(1), pc.max(1), new_id, identity, trust_mode(), flash) {
        Ok(pane) => {
            w.panes.push(pane);
            w.next_id += 1;
            w.tree.split(dir, new_id);
            w.zoomed = false; // a fresh split is always tiled
        }
        Err(e) => {
            *flash = Some((
                format!("cannot start {:?}: {e}", command[0]),
                Instant::now(),
            ));
            let _ = rows;
            let _ = cols;
        }
    }
}

/// Recompute every pane's rect and push the size to its pty and emulator. In
/// passthrough (single/zoomed) the focused pane owns the whole area; in tiled
/// mode each pane gets its rect.
fn resize_window(w: &mut Window, rows: u16, cols: u16) {
    if w.tiled() {
        let outer = tiled_outer(rows, cols);
        let rects = w.tree.rects(outer);
        for (id, rect) in rects {
            // Each pane wears a one-cell box border on every side, so its child
            // and emulator see the *inner* content area, not the bordered rect.
            let inner_rows = rect.rows.saturating_sub(2).max(1);
            let inner_cols = rect.cols.saturating_sub(2).max(1);
            if let Some(p) = w.pane_mut(id) {
                let _ = p.pty.resize(inner_rows as u16, inner_cols as u16);
                p.term.resize(inner_rows, inner_cols);
            }
        }
    } else {
        // Passthrough: the focused (or sole) pane fills the area above the bar.
        let ar = rows.saturating_sub(1).max(1);
        let focus = w.tree.focus();
        let sole = w.panes.len() == 1;
        for p in w.panes.iter_mut() {
            if p.id == focus || sole {
                let _ = p.pty.resize(ar, cols);
                p.term.resize(ar as usize, cols as usize);
            }
        }
    }
}

fn switch_window(
    windows: &mut [Window],
    active: &mut usize,
    to: usize,
    rows: u16,
    cols: u16,
    out: &mut impl Write,
) {
    if to == *active {
        return;
    }
    *active = to;
    for p in windows[to].panes.iter_mut() {
        p.activity = false;
    }
    // Re-assert the bar-protecting scroll region (rows-1) before clearing: a
    // passthrough app (claude) may have changed or reset the real terminal's
    // scroll region while it ran, and without restoring it the next pane can
    // scroll into the bar row.
    let _ = write!(out, "\x1b[1;{}r\x1b[2J\x1b[H", rows.saturating_sub(1).max(1));
    let _ = out.flush();
    resize_window(&mut windows[to], rows, cols);
}

/// Passthrough repaint: nudge the focused pane's pty size so its terminal
/// repaints in full (ConPTY always does; Unix full-screen apps redraw on
/// SIGWINCH). Used on window switch and mode changes into passthrough.
fn repaint_focused(w: &mut Window, rows: u16, cols: u16, out: &mut impl Write) {
    // Re-assert the bar-protecting scroll region (see `switch_window`) before the
    // repaint nudge, so the refreshed pane stays out of the bar row.
    let _ = write!(out, "\x1b[1;{}r\x1b[2J\x1b[H", rows.saturating_sub(1).max(1));
    let _ = out.flush();
    let ar = rows.saturating_sub(1).max(1);
    let focus = w.tree.focus();
    if let Some(p) = w.pane_mut(focus) {
        let _ = p.pty.resize(ar.saturating_sub(1).max(1), cols);
        let _ = p.pty.resize(ar, cols);
    }
}

/// The `amux fleet …` command family. `fleet up <name>` brings up a saved
/// roster; `fleet ls` lists the fleet names; anything else prints usage. Kept
/// separate from the hosted-program path — a fleet is amux's own command, not a
/// child to run.
fn fleet_cmd(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("up") => match args.get(1) {
            Some(name) => fleet_up(name),
            None => {
                eprintln!("amux fleet up <name>: needs a fleet name (try `amux fleet ls`)");
                ExitCode::FAILURE
            }
        },
        Some("ls") => fleet_ls(),
        _ => {
            eprintln!("usage: amux fleet up <name> | amux fleet ls");
            ExitCode::FAILURE
        }
    }
}

/// List the fleet names in the discovered fleet file, in file order. A missing
/// file or a malformed one is a clear error on stderr (non-zero exit).
fn fleet_ls() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match amux::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("amux fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match amux::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("amux fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let names = fleets.names();
    if names.is_empty() {
        println!("(no fleets defined in {})", located.path.display());
    } else {
        for name in names {
            println!("{name}");
        }
    }
    ExitCode::SUCCESS
}

/// `amux fleet up <name>` — read the fleet file, build one tiled window with a
/// pane per agent (each in its `cwd`, under its identity, with its extra args),
/// and hand it to the run loop. Any error before spawning (no file, bad JSON,
/// unknown name, empty fleet, a bad grid, a missing `cwd`) is reported and
/// **nothing is spawned** — never a partial fleet.
fn fleet_up(name: &str) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let located = match amux::fleet::discover(&cwd) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("amux fleet: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&located.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux fleet: cannot read {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleets = match amux::fleet::parse(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("amux fleet: {}: {e}", located.path.display());
            return ExitCode::FAILURE;
        }
    };
    let fleet = match fleets.get(name) {
        Some(f) => f.clone(),
        None => {
            let available = fleets.names().join(", ");
            eprintln!("amux fleet: no fleet named {name:?} (available: {available})");
            return ExitCode::FAILURE;
        }
    };

    // Grid: explicit `RxC` (must fit the agent count) or an auto balanced grid.
    let n = fleet.agents.len();
    let grid = match &fleet.grid {
        Some(spec) => match amux::spawn::Grid::parse_spec(spec) {
            Ok(g) if g.total() >= n => g,
            Ok(g) => {
                eprintln!(
                    "amux fleet: grid {spec:?} has {} cells but fleet {name:?} has {n} agents",
                    g.total()
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("amux fleet: fleet {name:?}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => amux::spawn::Grid::balanced(n.max(2)),
    };

    // Validate every agent's cwd *before* spawning anything, so a bad path never
    // leaves a half-open fleet.
    for a in &fleet.agents {
        if let Some(dir) = &a.cwd {
            let resolved = amux::fleet::resolve_dir(&located.dir, dir);
            if !resolved.is_dir() {
                eprintln!(
                    "amux fleet: agent {:?}: cwd {} does not exist",
                    a.name,
                    resolved.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: stdin/stdout must be a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (rows, cols) = term.size().unwrap_or((24, 80));

    let mut flash: Option<(String, Instant)> = None;
    let window = match spawn_fleet_window(&fleet, &located.dir, grid, rows, cols, &mut flash) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("amux fleet: cannot start fleet {name:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // New panes / splits opened later host a shell under the fleet's default
    // identity — a scratch pane in-role, not another copy of an agent.
    let scratch = vec![default_shell()];
    // ctl is opt-in via the `amux --allow-ctl` path; the fleet path runs without
    // it in C1 (a fleet + live ctl-spawn combination lands later).
    run(
        &mut term,
        &scratch,
        fleet.identity.as_deref(),
        None,
        Some(window),
        false,
        amux::ctl::DEFAULT_MAX_DEPTH,
        amux::ctl::TrustMode::Off,
    )
}

/// Build the fleet's tiled window: one pane per agent, laid out on `grid`
/// (agents fill leaf ids `0..n` row-major). Each pane runs the agent's `cmd`
/// plus its fleet args (`--add-dir`/`--append-system-prompt`/`--model`/
/// `--effort`), under its identity (per-agent, else the fleet default), in its
/// resolved `cwd`. If any agent fails to spawn, the panes already started are
/// killed and the whole window is abandoned — never a partial fleet.
fn spawn_fleet_window(
    fleet: &amux::fleet::Fleet,
    base_dir: &std::path::Path,
    grid: amux::spawn::Grid,
    rows: u16,
    cols: u16,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let tree = Tree::grid(grid.rows, grid.cols);
    // Rough per-cell inner size (minus the one-cell border on each side); the
    // caller's resize_window fixes it exactly right after.
    let cell_rows = ((rows.saturating_sub(1).max(1) as usize / grid.rows.max(1)).saturating_sub(2))
        .max(1) as u16;
    let cell_cols = ((cols as usize / grid.cols.max(1)).saturating_sub(2)).max(1) as u16;

    let mut panes: Vec<Pane> = Vec::with_capacity(fleet.agents.len());
    for (id, agent) in fleet.agents.iter().enumerate() {
        // Resolve add_dirs against the fleet file's directory (absolute as-is).
        let resolved_dirs: Vec<String> = agent
            .add_dirs
            .iter()
            .map(|d| {
                amux::fleet::resolve_dir(base_dir, d)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let command = agent.args(&resolved_dirs);
        // Per-agent identity else the fleet default.
        let identity = agent.identity.as_deref().or(fleet.identity.as_deref());
        // cwd resolved against the fleet dir (validated to exist by the caller).
        let cwd = agent.cwd.as_ref().map(|d| {
            amux::fleet::resolve_dir(base_dir, d)
                .to_string_lossy()
                .into_owned()
        });
        match spawn_pane_full(
            &command,
            cell_rows,
            cell_cols,
            id,
            identity,
            cwd.as_deref(),
            trust_mode(),
            flash,
        ) {
            Ok(pane) => panes.push(pane),
            Err(e) => {
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return Err(e);
            }
        }
    }
    Ok(Window {
        panes,
        tree,
        zoomed: false,
        next_id: fleet.agents.len(),
    })
}

/// The agsess status label the ctl protocol reports (stable strings the calling
/// agent can match on).
fn status_label(s: agsess::Status) -> &'static str {
    match s {
        agsess::Status::Working => "working",
        agsess::Status::WaitingApproval => "waiting-approval",
        agsess::Status::WaitingPrompt => "waiting-prompt",
        agsess::Status::Idle => "idle",
    }
}

/// Apply one ctl request against the live window set and return the JSON reply
/// line, **recording it to the audit log** (design §5). Thin wrapper: parse,
/// serve `audit` reads directly (they need the log and are not self-recorded),
/// else [`dispatch_ctl`] the request and record its outcome. `spawn`/`send`/
/// `status`/`kill` with a target are subtree-scoped; `spawn --identity` is
/// delegation-scoped.
#[allow(clippy::too_many_arguments)]
fn apply_ctl(
    line: &str,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &agsess::World,
    session_identity: Option<&str>,
    audit: &mut amux::audit::Audit,
    board: &mut amux::board::Board,
    bus: &mut amux::bus::Bus,
) -> String {
    use amux::ctl::{self, Cmd};

    let req = match ctl::parse_request(line) {
        Ok(r) => r,
        Err(e) => {
            let reply = ctl::reply_err(&e);
            audit.record(None, "bad-request", &truncate(line, 80), false, &e);
            return reply;
        }
    };
    let caller = req.caller;
    let privileged = caller_privileged(windows, caller);

    // `audit` is served here (it reads the log) and is not itself recorded — a
    // query of the log shouldn't pollute the log. It is subtree-scoped like any
    // read: a worker sees only its own subtree's entries.
    if let Cmd::Audit(ar) = &req.cmd {
        return audit_reply(audit, windows, caller, privileged, ar.tail);
    }

    let (action, detail) = audit_label(&req);
    let reply = dispatch_ctl(
        req,
        windows,
        rows,
        cols,
        max_depth,
        extra_allow,
        pending,
        world,
        session_identity,
        privileged,
        board,
        bus,
    );
    let (ok, note) = audit_outcome(&reply);
    audit.record(caller, action, &detail, ok, &note);
    reply
}

/// The server side of the control channel: turn one parsed [`amux::ctl::Request`]
/// into its JSON reply. `list`/`status` serialize the spawn tree (agsess status
/// folded in); `spawn` opens a visible worker after the pure allowlist/depth
/// guard ([`amux::ctl::evaluate_spawn`]) and the credential-delegation guard
/// ([`amux::ctl::delegation_allowed`]); `send` enqueues a queue-until-idle
/// delivery; `kill` tears down the target's subtree (the reap step reaps them).
/// `send`/`status`/`kill` with a target are subtree-scoped.
#[allow(clippy::too_many_arguments)]
fn dispatch_ctl(
    req: amux::ctl::Request,
    windows: &mut Vec<Window>,
    rows: u16,
    cols: u16,
    max_depth: usize,
    extra_allow: &[String],
    pending: &mut Vec<PendingSend>,
    world: &agsess::World,
    session_identity: Option<&str>,
    privileged: bool,
    board: &mut amux::board::Board,
    bus: &mut amux::bus::Bus,
) -> String {
    use amux::ctl::{self, Cmd};
    let caller = req.caller;

    match req.cmd {
        Cmd::List => reply_tree(windows, world, None),
        Cmd::Status(sr) => match sr.target {
            None => {
                // No target: the caller's subtree (whole tree for the operator).
                let root = if privileged { None } else { caller };
                reply_tree(windows, world, root)
            }
            Some(t) => {
                let candidates = ctl_candidates(windows);
                let id = match ctl::resolve_target(&t, &candidates) {
                    Ok(id) => id,
                    Err(e) => return ctl::reply_err(&e),
                };
                if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                    return deny;
                }
                let status = pane_by_agent(windows, id).and_then(|p| {
                    amux::bind::status_for(p.session_id.as_deref(), &world.sessions)
                        .map(status_label)
                });
                ctl::reply_status_one(id, status)
            }
        },
        Cmd::Send(sr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&sr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Queued iff the target is mid-turn now; either way delivery is async
            // and happens when the target is idle.
            let busy = matches!(
                pane_by_agent(windows, id)
                    .and_then(|p| amux::bind::status_for(p.session_id.as_deref(), &world.sessions)),
                Some(agsess::Status::Working) | Some(agsess::Status::WaitingApproval)
            );
            pending.push(PendingSend {
                target: id,
                text: sr.text,
                queued_at: Instant::now(),
                text_written_at: None,
            });
            ctl::reply_sent(id, busy)
        }
        Cmd::Spawn(mut sp) => {
            // amux owns the permission posture. Two layers, both surfaced (never
            // silent) in the reply `note`:
            //
            //  1. Strip RAW claude permission flags the agent slipped into the argv
            //     (`--dangerously-skip-permissions`, `--permission-mode`, …). Agents
            //     request a mode through `--mode`, not raw flags, so amux stays the
            //     single source of truth.
            //  2. Resolve the effective mode from the per-spawn `--mode` under the
            //     session policy: the **operator** (the human's root pane) may set
            //     ANY mode (elevation is the human directing); a non-operator
            //     **worker** is capped at the policy — it may match or de-escalate
            //     but never elevate itself. No `--mode` ⇒ inherit the policy.
            let (cleaned_argv, stripped) = ctl::sanitize_spawn_argv(&sp.argv);
            sp.argv = cleaned_argv;
            let mut notes: Vec<String> = Vec::new();
            if !stripped.is_empty() {
                notes.push(format!(
                    "ignored raw {} — request a mode with --mode instead",
                    stripped.join(", ")
                ));
            }
            let policy = trust_mode();
            let effective = match sp.mode {
                None => policy,
                Some(req) if privileged => req,
                Some(req) if req.rank() <= policy.rank() => req,
                Some(req) => {
                    notes.push(format!(
                        "capped teammate to {} (session policy); a worker cannot elevate itself to {}",
                        policy.policy_label(),
                        req.policy_label()
                    ));
                    policy
                }
            };
            let note = if notes.is_empty() {
                None
            } else {
                Some(notes.join("; "))
            };
            let note = note.as_deref();
            let caller_depth = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| p.depth)
                .unwrap_or(0);
            let new_depth =
                match ctl::evaluate_spawn(&sp.argv, caller_depth, max_depth, extra_allow) {
                    Ok(d) => d,
                    Err(denied) => return ctl::reply_err(&denied.message()),
                };
            // Credential-delegation guard (§5): a worker may only pass down an
            // identity it itself holds (its own or the session default); the
            // operator delegates anything.
            let caller_identity = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .and_then(|p| p.identity.clone());
            if let Err(msg) = ctl::delegation_allowed(
                sp.identity.as_deref(),
                caller_identity.as_deref(),
                session_identity,
                privileged,
            ) {
                return ctl::reply_err(&msg);
            }
            if sp.new_window {
                spawn_worker_window(windows, &sp, caller, new_depth, rows, cols, effective, note)
            } else {
                spawn_worker_here(windows, &sp, caller, new_depth, rows, cols, effective, note)
            }
        }
        Cmd::Kill(kr) => {
            let candidates = ctl_candidates(windows);
            let id = match ctl::resolve_target(&kr.target, &candidates) {
                Ok(id) => id,
                Err(e) => return ctl::reply_err(&e),
            };
            if let Some(deny) = scope_denied(windows, caller, privileged, id) {
                return deny;
            }
            // Tear down the target and every descendant. We only mark the panes
            // dead (kill the pty, set `exited`); the run loop's reap step (§4)
            // then collapses the split trees, drops the panes, and removes any
            // window left empty — the same proven path an interactive `x` uses.
            let parents = ctl_parents(windows);
            let mut killed: Vec<usize> = Vec::new();
            for w in windows.iter_mut() {
                for p in w.panes.iter_mut() {
                    if amux::ctl::in_subtree(p.agent_id, id, &parents) {
                        let _ = p.pty.kill();
                        p.exited = true;
                        killed.push(p.agent_id);
                    }
                }
            }
            killed.sort_unstable();
            ctl::reply_killed(&killed)
        }
        // `audit` is handled in `apply_ctl` (it needs the log); never reaches here.
        Cmd::Audit(_) => ctl::reply_err("internal: audit dispatched to the wrong handler"),
        Cmd::Board(op) => {
            // The board is SHARED team state: any pane in the session reads and
            // writes the same source of truth (no subtree scoping — coordination
            // is the team's job, and `updated_by` + the audit log keep it
            // accountable). Single-writer daemon ⇒ no locking.
            let by = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id + 1))
                })
                .or_else(|| Some("operator".to_string()));
            match op {
                ctl::BoardOp::Set { key, fields } => {
                    let e = board.set(&key, &fields, by.as_deref(), agsess::sessions::now_ms());
                    ctl::reply_board_entry(&key, Some(amux::board::entry_to_value(&e)))
                }
                ctl::BoardOp::Get { key } => {
                    let entry = board.get(&key).map(amux::board::entry_to_value);
                    ctl::reply_board_entry(&key, entry)
                }
                ctl::BoardOp::List => {
                    ctl::reply_board_list(amux::board::entries_to_value(&board.list()))
                }
                ctl::BoardOp::Del { key } => {
                    let deleted = board.del(&key);
                    ctl::reply_board_del(&key, deleted)
                }
            }
        }
        Cmd::Bus(op) => {
            // The bus is SHARED like the board. The server derives `who` (the
            // caller's role, else its pane, else the operator) so a worker
            // publishes and subscribes *as itself* — it cannot forge another
            // sender. `who` is stable per pane, so the no-echo filter and a
            // subscriber's cursor stay consistent across calls.
            let who = caller
                .and_then(|cid| pane_by_agent(windows, cid))
                .map(|p| {
                    p.role
                        .clone()
                        // 1-based to match the status bar's `1:`, `2:` numbering
                        // (agent_id is 0-based internally). Roles are the stable
                        // identity; this is the human-friendly fallback label.
                        .unwrap_or_else(|| format!("pane {}", p.agent_id + 1))
                })
                .unwrap_or_else(|| "operator".to_string());
            let now = agsess::sessions::now_ms();
            match op {
                ctl::BusOp::Pub { topic, kind, fields } => {
                    match bus.publish(&topic, kind, Some(&who), &fields, now) {
                        Ok(e) => ctl::reply_bus_published(amux::bus::event_to_value(&e)),
                        Err(msg) => ctl::reply_err(&msg),
                    }
                }
                ctl::BusOp::Sub { topics } => {
                    bus.subscribe(&who, &topics);
                    ctl::reply_bus_subscribed(current_subs(bus, &who))
                }
                ctl::BusOp::Unsub { topics } => {
                    bus.unsubscribe(&who, &topics);
                    ctl::reply_bus_subscribed(current_subs(bus, &who))
                }
                ctl::BusOp::Feed { since } => {
                    let events = bus.feed(&who, since);
                    // The new cursor is the max seq pulled, or `since` when empty,
                    // so it never rewinds.
                    let cursor = events.iter().map(|e| e.seq).max().unwrap_or(since);
                    ctl::reply_bus_feed(amux::bus::events_to_value(&events), cursor)
                }
                ctl::BusOp::Resolve { seq } => {
                    ctl::reply_bus_resolved(seq, bus.resolve(seq))
                }
            }
        }
    }
}

/// A subscriber's current topic set as a `Vec` (for the `sub`/`unsub` reply).
fn current_subs(bus: &amux::bus::Bus, who: &str) -> Vec<String> {
    bus.subscriptions(who)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Truncate a string to `n` chars (char-safe), appending `…` when cut. Keeps a
/// bad-request line short in the audit log.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// The audit `(action, detail)` for a request: a stable verb plus a compact,
/// **secret-free** description (identity *names* only; `send` logs the text
/// *length*, never the body).
fn audit_label(req: &amux::ctl::Request) -> (&'static str, String) {
    use amux::ctl::Cmd;
    match &req.cmd {
        Cmd::List => ("list", String::new()),
        Cmd::Status(sr) => (
            "status",
            match &sr.target {
                Some(t) => format!("target={t}"),
                None => "scope=subtree".to_string(),
            },
        ),
        Cmd::Send(sr) => (
            "send",
            format!("target={} len={}", sr.target, sr.text.chars().count()),
        ),
        Cmd::Spawn(sp) => {
            let stem = sp
                .argv
                .first()
                .map(|a| {
                    std::path::Path::new(a)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| a.clone())
                })
                .unwrap_or_default();
            (
                "spawn",
                format!(
                    "role={} argv={} identity={}",
                    sp.role.as_deref().unwrap_or("-"),
                    stem,
                    sp.identity.as_deref().unwrap_or("-")
                ),
            )
        }
        Cmd::Kill(kr) => ("kill", format!("target={}", kr.target)),
        Cmd::Audit(_) => ("audit", String::new()),
        Cmd::Board(op) => (
            "board",
            match op {
                amux::ctl::BoardOp::Set { key, fields } => {
                    format!("set {key} fields={}", fields.len())
                }
                amux::ctl::BoardOp::Get { key } => format!("get {key}"),
                amux::ctl::BoardOp::List => "list".to_string(),
                amux::ctl::BoardOp::Del { key } => format!("del {key}"),
            },
        ),
        Cmd::Bus(op) => (
            "bus",
            match op {
                amux::ctl::BusOp::Pub { topic, kind, fields } => {
                    format!("pub {topic} {} fields={}", kind.as_str(), fields.len())
                }
                amux::ctl::BusOp::Sub { topics } => format!("sub {}", topics.join(",")),
                amux::ctl::BusOp::Unsub { topics } => format!("unsub {}", topics.join(",")),
                amux::ctl::BusOp::Feed { since } => format!("feed since={since}"),
                amux::ctl::BusOp::Resolve { seq } => format!("resolve {seq}"),
            },
        ),
    }
}

/// Read `(ok, note)` back out of a reply line for the audit record: the error on
/// failure, or a compact success tag naming the salient id(s).
fn audit_outcome(reply: &str) -> (bool, String) {
    let v = match json::parse(reply) {
        Ok(v) => v,
        Err(_) => return (false, "unparseable reply".to_string()),
    };
    let ok = v.get("ok").and_then(json::Value::as_bool).unwrap_or(false);
    if !ok {
        return (
            false,
            v.get("err")
                .and_then(json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    let note = if let Some(p) = v.get("pane").and_then(json::Value::as_i64) {
        format!("pane={p}")
    } else if let Some(k) = v.get("killed").and_then(json::Value::as_array) {
        let ids: Vec<String> = k
            .iter()
            .filter_map(json::Value::as_i64)
            .map(|n| n.to_string())
            .collect();
        format!("killed={}", ids.join(","))
    } else if let Some(t) = v.get("target").and_then(json::Value::as_i64) {
        format!("target={t}")
    } else {
        String::new()
    };
    (true, note)
}

/// Serve a `ctl audit` read: the in-memory log, subtree-scoped. The operator
/// sees everything; a worker sees only entries issued from within its own
/// subtree (operator/`None`-caller entries are hidden from it).
fn audit_reply(
    audit: &amux::audit::Audit,
    windows: &[Window],
    caller: Option<usize>,
    privileged: bool,
    tail: Option<usize>,
) -> String {
    let entries = if privileged {
        audit.view(tail, |_| true)
    } else if let Some(root) = caller {
        let parents = ctl_parents(windows);
        audit.view(tail, |e| match e.caller {
            Some(c) => amux::ctl::in_subtree(c, root, &parents),
            None => false, // operator actions are hidden from a worker
        })
    } else {
        audit.view(tail, |_| true)
    };
    amux::ctl::reply_audit(entries)
}

/// Serialize the spawn tree as a `list`/`status` reply. `root == Some(id)` limits
/// it to that pane's subtree (subtree-scoped status); `None` is the whole tree.
fn reply_tree(windows: &[Window], world: &agsess::World, root: Option<usize>) -> String {
    let parents = ctl_parents(windows);
    let mut panes: Vec<&Pane> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .filter(|p| match root {
            None => true,
            Some(r) => amux::ctl::in_subtree(p.agent_id, r, &parents),
        })
        .collect();
    panes.sort_by_key(|p| p.agent_id);
    let nodes: Vec<amux::ctl::TreeNode> = panes
        .iter()
        .map(|p| amux::ctl::TreeNode {
            id: p.agent_id,
            parent: p.parent,
            role: p.role.as_deref(),
            title: &p.title,
            depth: p.depth,
            status: amux::bind::status_for(p.session_id.as_deref(), &world.sessions)
                .map(status_label),
        })
        .collect();
    amux::ctl::reply_list(&nodes)
}

/// Subtree-scope guard: `None` if the caller may act on `target`, else a ready
/// JSON refusal. The operator (privileged) may act on anything.
fn scope_denied(
    windows: &[Window],
    caller: Option<usize>,
    privileged: bool,
    target: usize,
) -> Option<String> {
    if privileged {
        return None;
    }
    let root = caller?; // non-privileged implies a known caller pane
    if amux::ctl::in_subtree(target, root, &ctl_parents(windows)) {
        None
    } else {
        Some(amux::ctl::reply_err(&format!(
            "pane {target} is outside your subtree; a worker may only steer what it spawned"
        )))
    }
}

/// `ctl spawn` (default): a visible worker in a brand-new window.
#[allow(clippy::too_many_arguments)]
fn spawn_worker_window(
    windows: &mut Vec<Window>,
    sp: &amux::ctl::SpawnReq,
    caller: Option<usize>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: amux::ctl::TrustMode,
    note: Option<&str>,
) -> String {
    let mut flash = None;
    match spawn_window(
        &sp.argv,
        rows,
        cols,
        windows.len(),
        sp.identity.as_deref(),
        mode,
        &mut flash,
    ) {
        Ok(mut w) => {
            let pane = &mut w.panes[0];
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            windows.push(w);
            // Size the new window's pane to its true rect now, so the agent
            // paints full-height immediately instead of at the rough spawn size
            // (which otherwise needs a manual terminal resize to correct).
            let last = windows.len() - 1;
            resize_window(&mut windows[last], rows, cols);
            amux::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => amux::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// `ctl spawn --here`: tile the worker *beside* the caller, in the caller's own
/// window, so a lead and its ICs sit in one view. Falls back to an error if the
/// caller's pane can't be located (nothing to sit beside).
#[allow(clippy::too_many_arguments)]
fn spawn_worker_here(
    windows: &mut [Window],
    sp: &amux::ctl::SpawnReq,
    caller: Option<usize>,
    new_depth: usize,
    rows: u16,
    cols: u16,
    mode: amux::ctl::TrustMode,
    note: Option<&str>,
) -> String {
    let Some(caller_id) = caller else {
        return amux::ctl::reply_err(
            "`--here` needs a caller pane; run it from inside an amux pane",
        );
    };
    // Locate the window holding the caller and that caller's per-window pane id.
    let Some((wi, caller_pane_id)) = windows.iter().enumerate().find_map(|(i, w)| {
        w.panes
            .iter()
            .find(|p| p.agent_id == caller_id)
            .map(|p| (i, p.id))
    }) else {
        return amux::ctl::reply_err("`--here`: caller pane not found (rerun without --here)");
    };

    let w = &mut windows[wi];
    let new_id = w.next_id;
    // Rough half-cell inner size; the caller resizes the window right after.
    let (pr, pc) = (
        (rows.saturating_sub(1).max(1) / 2).saturating_sub(2),
        (cols / 2).saturating_sub(2),
    );
    let mut flash = None;
    match spawn_pane(
        &sp.argv,
        pr.max(1),
        pc.max(1),
        new_id,
        sp.identity.as_deref(),
        mode,
        &mut flash,
    ) {
        Ok(mut pane) => {
            pane.role = sp.role.clone();
            pane.parent = caller;
            pane.depth = new_depth;
            let agent_id = pane.agent_id;
            let session = pane.session_id.clone();
            w.panes.push(pane);
            w.next_id += 1;
            // Re-tile the WHOLE window into a balanced near-square grid over all
            // its panes (keeping their stable ids), rather than just splitting the
            // caller side-by-side. A plain `split_pane` each time stacks every
            // `--here` worker into one column, so N of them degrade to an
            // unusable `1×N` strip; re-gridding keeps 2→1×2, 4→2×2, 6→2×3,
            // 12→3×4, … balanced. Focus lands on the fresh worker.
            let _ = caller_pane_id; // (kept for the error message above)
            let ids: Vec<usize> = w.panes.iter().map(|p| p.id).collect();
            w.tree = Tree::grid_from_ids(&ids);
            w.tree.focus_pane(new_id);
            w.zoomed = false;
            // Resize the whole window so both the caller and the fresh worker get
            // their exact inner rects — without this the worker keeps its rough
            // half-size and paints short (blank below), fixed only by a manual
            // terminal resize. Mirrors what the interactive split handlers do.
            resize_window(&mut windows[wi], rows, cols);
            amux::ctl::reply_spawned(agent_id, sp.role.as_deref(), session.as_deref(), note)
        }
        Err(e) => amux::ctl::reply_err(&format!("spawn failed: {e}")),
    }
}

/// Flush queued `ctl send`s (Decision 4: queue until the target is idle). For
/// each pending send: once the target reports a ready status (or the unbound
/// fallback elapses), write the text; a beat later write the Enter and drop it.
/// A vanished target is dropped. Returns whether anything was written (so the
/// caller can request a repaint).
fn flush_sends(
    pending: &mut Vec<PendingSend>,
    windows: &mut [Window],
    world: &agsess::World,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut wrote = false;
    pending.retain_mut(|ps| {
        match ps.text_written_at {
            None => {
                // Decide readiness from the target's live status.
                let ready = match pane_by_agent(windows, ps.target) {
                    None => return false, // target gone — drop the send
                    Some(p) => {
                        match amux::bind::status_for(p.session_id.as_deref(), &world.sessions) {
                            Some(agsess::Status::WaitingPrompt) | Some(agsess::Status::Idle) => {
                                true
                            }
                            Some(_) => false, // Working / WaitingApproval — keep waiting
                            None => now.duration_since(ps.queued_at) >= SEND_UNBOUND_FALLBACK,
                        }
                    }
                };
                if ready {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        let _ = p.pty.write(ps.text.as_bytes());
                        ps.text_written_at = Some(now);
                        wrote = true;
                    } else {
                        return false;
                    }
                }
                true
            }
            Some(t) => {
                if now.duration_since(t) >= SEND_ENTER_DELAY {
                    if let Some(p) = pane_by_agent_mut(windows, ps.target) {
                        let _ = p.pty.write(b"\r");
                        wrote = true;
                    }
                    false // delivered — drop
                } else {
                    true
                }
            }
        }
    });
    wrote
}

fn spawn_window(
    command: &[String],
    rows: u16,
    cols: u16,
    _idx: usize,
    identity: Option<&str>,
    mode: amux::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let pane = spawn_pane(
        command,
        rows.saturating_sub(1).max(1),
        cols,
        0,
        identity,
        mode,
        flash,
    )?;
    Ok(Window {
        panes: vec![pane],
        tree: Tree::new(0),
        zoomed: false,
        next_id: 1,
    })
}

/// Mass-spawn a single window of N tiles laid out as a balanced `grid`. The
/// split tree is built with [`layout::Tree::grid`] (ids `0..N`, focus 0) so
/// focus/rects/close all keep working exactly as for a hand-split grid. Each
/// tile runs the SAME command, each its own session (a fresh `--session-id` per
/// pane via the existing `bind` path inside `spawn_pane`), all under the same
/// `identity` if one was given. Panes are spawned at a rough grid-cell size; the
/// caller resizes the window right after so each pty/emulator gets its exact
/// inner rect. If any pane fails to spawn, the whole window is abandoned and the
/// already-spawned panes are killed (nothing to host half a grid).
fn spawn_window_grid(
    command: &[String],
    rows: u16,
    cols: u16,
    grid: amux::spawn::Grid,
    identity: Option<&str>,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Window> {
    let tree = Tree::grid(grid.rows, grid.cols);
    let n = grid.total();
    // Rough per-cell inner size (minus the one-cell border on each side); the
    // caller's resize_window fixes it exactly right after.
    let cell_rows = ((rows.saturating_sub(1).max(1) as usize / grid.rows.max(1)).saturating_sub(2))
        .max(1) as u16;
    let cell_cols = ((cols as usize / grid.cols.max(1)).saturating_sub(2)).max(1) as u16;
    let mut panes: Vec<Pane> = Vec::with_capacity(n);
    for id in 0..n {
        match spawn_pane(command, cell_rows, cell_cols, id, identity, trust_mode(), flash) {
            Ok(pane) => panes.push(pane),
            Err(e) => {
                // Tear down whatever we already started — a partial grid is not
                // a coherent window.
                for p in panes.iter_mut() {
                    let _ = p.pty.kill();
                }
                return Err(e);
            }
        }
    }
    Ok(Window {
        panes,
        tree,
        zoomed: false,
        next_id: n,
    })
}

fn spawn_pane(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    mode: amux::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    // The single-pane / split / grid path: no per-pane working directory (the
    // child inherits amux's cwd, today's behavior). The fleet loader is the only
    // caller that supplies a `cwd`; everyone else routes through here with `None`.
    spawn_pane_full(command, rows, cols, id, identity, None, mode, flash)
}

/// The shared spawn core: build the agent's launch (session-id inject, Windows
/// shim wrapping, identity env resolution), then spawn it on a pty of the given
/// size in the given working directory. Every pane amux hosts — the initial one,
/// a split, a grid tile, and each fleet agent — is born here, so the identity /
/// `--session-id` / effective-command discipline is written once and shared.
///
/// `command` is the *base* user command with any extra agent args already
/// appended (e.g. the fleet loader's `--add-dir` / `--append-system-prompt` /
/// `--model` / `--effort`); this function then appends `--session-id` when the
/// pane is a bindable agent, exactly as before. `cwd` is `Some(dir)` for a fleet
/// agent (so its `CLAUDE.md` auto-loads) and `None` everywhere else.
#[allow(clippy::too_many_arguments)]
fn spawn_pane_full(
    command: &[String],
    rows: u16,
    cols: u16,
    id: usize,
    identity: Option<&str>,
    cwd: Option<&str>,
    mode: amux::ctl::TrustMode,
    flash: &mut Option<(String, Instant)>,
) -> std::io::Result<Pane> {
    let title = std::path::Path::new(&command[0])
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| command[0].clone());
    // Permission posture for an agent pane (never a shell pane):
    //   Edits (`--trust`)          → `--permission-mode acceptEdits` + a safe
    //                                dev-command allowlist; dangerous commands
    //                                still prompt, visibly, in the pane.
    //   Auto (`automode`)          → `--permission-mode auto` (claude's auto mode:
    //                                hands-off edits + commands, its own guardrails).
    //   Skip (`skip`)              → `--dangerously-skip-permissions` (full bypass;
    //                                the human confirmed it at launch).
    //   Plan (`plan`)              → `--permission-mode plan` (read-only).
    // `mode` is the *effective* mode for this pane: the session policy for the
    // panes amux opens itself, or — for a ctl spawn — the per-spawn `--mode` after
    // the operator-elevate / worker-cap governance in `apply_ctl`.
    let is_agent = amux::bind::is_agent_stem(&title);
    let trusted_launch = is_agent && mode != amux::ctl::TrustMode::Off;
    let mut base: Vec<String> = if trusted_launch {
        let mut v = command.to_vec();
        match mode {
            amux::ctl::TrustMode::Edits => {
                v.extend(amux::trust::accept_edits_args(&amux::trust::extra_allow_from_env()));
            }
            amux::ctl::TrustMode::Auto => {
                v.push("--permission-mode".to_string());
                v.push("auto".to_string());
            }
            amux::ctl::TrustMode::Skip => {
                v.push(amux::ctl::SKIP_PERMISSIONS_FLAG.to_string());
            }
            amux::ctl::TrustMode::Plan => {
                v.push("--permission-mode".to_string());
                v.push("plan".to_string());
            }
            amux::ctl::TrustMode::Off => unreachable!("trusted_launch implies not Off"),
        }
        v
    } else {
        command.to_vec()
    };
    // When the ctl channel is live, teach every agent pane — the initial one and
    // ctl-spawned workers alike — to delegate through `amux ctl` (visible panes)
    // instead of its own invisible Task/background-agents tool. This is the
    // dependable layer: it is always in the agent's context (via
    // `--append-system-prompt`), so reliable delegation no longer hinges on a
    // skill happening to surface. Appended after any trust flags so it terminates
    // the `--allowedTools` list cleanly rather than being read as one of its values.
    if is_agent && CTL_ADDRESS.get().is_some() {
        base.push("--append-system-prompt".to_string());
        base.push(amux::ctl::AGENT_CTL_DIRECTIVE.to_string());
    }
    // …and pre-accept claude's *folder-trust* dialog for this pane's working
    // directory — a separate gate the permission mode does NOT cover (it's stored
    // per-dir in ~/.claude.json). Without this a trusted launch in an untrusted
    // folder still blocks on "trust this folder?". Only under --trust/--skip, only
    // the trust bit, only this pane's cwd; a parse/IO problem is flashed and the
    // pane spawns anyway (worst case: the dialog).
    if trusted_launch {
        let dir = cwd
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        if let Err(e) = amux::trust::ensure_trusted(&dir) {
            *flash = Some((format!("folder-trust: {e}"), Instant::now()));
        }
    }
    // Agent-aware bind (§3.3): if this is an agent pane amux is launching and the
    // user did not already pick a session, mint a uuid and append
    // `--session-id <uuid>` to the *agent's* args (before any `cmd /C` shim
    // wrapping, so the flag reaches claude, not the shim host). Remember the id.
    let session_id = amux::bind::session_id_for(&base);
    let mut user_cmd: Vec<String> = base;
    if let Some(uuid) = &session_id {
        user_cmd.push("--session-id".to_string());
        user_cmd.push(uuid.clone());
    }
    let effective = effective_command(&user_cmd);
    // Opt-in spawn diagnostic: `AMUX_SPAWN_LOG=<file>` appends the exact command
    // (post trust-flags, post shim) amux launches for each pane, so a "why isn't
    // this pane in the mode I expected" question is answered by data, not guesses.
    if let Ok(path) = std::env::var("AMUX_SPAWN_LOG") {
        if !path.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "[{title}] {}", effective.join(" "));
            }
        }
    }
    let argrefs: Vec<&str> = effective[1..].iter().map(String::as_str).collect();
    let r = rows.max(1);
    let c = cols.max(1);

    // Stamp the process-global agent id now — it is both the pane's spawn-tree
    // key and the `AMUX_PANE` value injected below, so an agent inside can
    // attribute its own `ctl spawn` calls back to this pane.
    let agent_id = next_agent_id();

    // ctl env (non-secret): when the control channel is on, every pane learns
    // the endpoint (`AMUX_CTL`) and its own id (`AMUX_PANE`). This is the base
    // env; identity secrets (if any) are merged on top for this one spawn.
    let mut base_env: Vec<(String, String)> = Vec::new();
    if let Some(addr) = CTL_ADDRESS.get() {
        base_env.push((amux::ctl::ENV_ADDRESS.to_string(), addr.clone()));
        base_env.push((amux::ctl::ENV_PANE.to_string(), agent_id.to_string()));
    }

    // Identity injection (path B): only for an agent pane with an identity set.
    // Decide ONCE so the spawn path and the pane's stored tag can never diverge
    // — a pane tagged with an identity is exactly a pane spawned with its env.
    let inject = amux::identity::wants_env(command, identity);

    // We RE-RESOLVE on every spawn — the resolved env (which contains secret
    // material) lives only in this local, is handed straight to the pty, and is
    // dropped at the end of this function. It is never cached on the pane,
    // never logged, never printed. The pane stores only the identity *name*.
    //
    // Resolve failure (no such target, vault locked) is surfaced in the bar and
    // the pane spawns with plain env (ambient creds) but still in `cwd` — visible,
    // not silent, and never unauthenticated-without-saying-so (§7).
    let pty = if inject {
        let name = identity.expect("wants_env implies Some");
        match akey::resolve(name) {
            Ok(env) => {
                // `env` holds secret values; merged with the (non-secret) ctl
                // base env for this one spawn, then dropped. Deliberately never
                // formatted, logged, or stored.
                let mut merged = base_env.clone();
                merged.extend(env);
                pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &merged, cwd)?
            }
            Err(e) => {
                // Name only in the message — `e` is akey's own error text
                // ("no key or WIF profile named …"), which carries the name the
                // user typed, never any secret value.
                *flash = Some((
                    format!("identity {name:?} unresolved: {e} — running without it"),
                    Instant::now(),
                ));
                pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &base_env, cwd)?
            }
        }
    } else {
        pty::Pty::spawn_full(&effective[0], &argrefs, r, c, &base_env, cwd)?
    };
    Ok(Pane {
        id,
        pty,
        term: vterm::Term::new(r as usize, c as usize),
        filter: amux::filter::Passthrough::new(),
        title,
        activity: false,
        exited: false,
        session_id,
        // Store the identity NAME only, and only for panes that would actually
        // run under it (agent panes) — a shell keeps `None`, so no misleading
        // tag on a pane that was never credentialed. `inject` is the same
        // decision the spawn used, so tag and env can never disagree.
        identity: identity.filter(|_| inject).map(str::to_string),
        agent_id,
        // Spawn-tree fields default to "root pane the human opened"; the ctl
        // spawn handler overrides role/parent/depth for a ctl-created worker.
        role: None,
        parent: None,
        depth: 0,
        painted: false,
        mouse_wanted: false,
    })
}

/// Sanitize the terminal on exit and leave the alt screen. amux owns the alt
/// buffer (§ the run loop's `\x1b[?1049h` at start), but a hosted app may have
/// left modes on — mouse reporting, bracketed paste, a hidden cursor, an
/// altered scroll region, a non-default SGR. Leaving the alt screen alone does
/// not undo those, so we reset them first, in a sensible order, before the
/// `?1049l` swap so the user's original shell comes back clean. Called on
/// **every** exit path in `run` (normal quit, last-pane-exit, read error, and
/// the initial-spawn failure).
fn cleanup_screen(out: &mut impl Write) {
    let _ = write!(
        out,
        // Reset SGR; disable mouse reporting (1000/1002/1003/1006); disable
        // bracketed paste (2004); show the cursor (25); reset the scroll region;
        // then leave the alt screen (1049) last so the swap-back is the final act.
        "\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[r\x1b[?1049l"
    );
    let _ = out.flush();
}

/// Diagnostic mode: print the hex of every raw byte stdin delivers for a few
/// seconds. Answers "what does my terminal actually send?" — including through
/// nested consoles — without guessing.
fn stdin_probe() -> ExitCode {
    let mut term = match rawterm::Terminal::raw() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("amux: not a terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "probe: type for 3s\r");
    let _ = out.flush();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Ok(bytes) = term.read_bytes(Duration::from_millis(100)) {
            if !bytes.is_empty() {
                let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let _ = writeln!(out, "got[{}]\r", hex.join(" "));
                let _ = out.flush();
            }
        }
    }
    let _ = writeln!(out, "probe-done\r");
    let _ = out.flush();
    ExitCode::SUCCESS
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
    }
}

/// The outcome of feeding a keystroke chunk to the open command prompt.
enum PromptEdit {
    /// The line changed (or nothing happened); keep the prompt open.
    Continue,
    /// Esc / Ctrl+C: close the prompt without running anything.
    Cancel,
    /// Enter: the caller runs the accumulated line.
    Submit,
}

/// Apply a chunk of raw input `bytes` to the prompt line `buf`: printable ASCII is
/// appended, Backspace deletes, Enter submits, Esc/Ctrl+C cancels. Other control
/// bytes (including the rest of an arrow-key escape) are ignored. Returns as soon
/// as a terminating key (Enter/Esc) is seen so trailing bytes don't leak.
fn edit_prompt(buf: &mut String, bytes: &[u8]) -> PromptEdit {
    for &b in bytes {
        match b {
            b'\r' | b'\n' => return PromptEdit::Submit,
            0x1b | 0x03 => return PromptEdit::Cancel, // Esc or Ctrl+C
            0x7f | 0x08 => {
                buf.pop();
            }
            0x20..=0x7e => buf.push(b as char),
            _ => {} // ignore other control bytes
        }
    }
    PromptEdit::Continue
}

/// Split a command line into argv, honoring double quotes so a path with spaces
/// stays one argument (`"C:\Program Files\Git\bin\bash.exe" --login`). Whitespace
/// separates unquoted words; quotes are removed. Minimal by design — enough to
/// launch a shell with a flag, not a full shell parser.
fn split_cmdline(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// On Windows, resolve the command the way the shell would (PATH x PATHEXT) and
/// host `.cmd`/`.bat` shims under `cmd /C` — npm-installed CLIs (Claude Code
/// included) are such shims, and `CreateProcessW` cannot launch them directly.
#[cfg(windows)]
fn effective_command(command: &[String]) -> Vec<String> {
    use amux::resolve;
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    match resolve::resolve(&command[0], &dirs, &exts) {
        Some(path) if resolve::needs_shell(&path) => {
            let mut v = vec!["cmd".to_string(), "/C".to_string()];
            v.push(path.to_string_lossy().into_owned());
            v.extend(command[1..].iter().cloned());
            v
        }
        Some(path) => {
            let mut v = vec![path.to_string_lossy().into_owned()];
            v.extend(command[1..].iter().cloned());
            v
        }
        None => command.to_vec(),
    }
}

#[cfg(not(windows))]
fn effective_command(command: &[String]) -> Vec<String> {
    command.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_cmdline_splits_on_whitespace() {
        assert_eq!(split_cmdline("wsl -d Ubuntu"), vec!["wsl", "-d", "Ubuntu"]);
        assert_eq!(split_cmdline("  pwsh   -NoLogo "), vec!["pwsh", "-NoLogo"]);
        assert!(split_cmdline("   ").is_empty());
    }

    #[test]
    fn split_cmdline_keeps_quoted_paths_whole() {
        assert_eq!(
            split_cmdline(r#""C:\Program Files\Git\bin\bash.exe" --login"#),
            vec![r"C:\Program Files\Git\bin\bash.exe", "--login"]
        );
    }

    #[test]
    fn edit_prompt_appends_backspaces_submits_and_cancels() {
        let mut b = String::new();
        assert!(matches!(edit_prompt(&mut b, b"wsl"), PromptEdit::Continue));
        assert_eq!(b, "wsl");
        // backspace deletes the last char
        assert!(matches!(edit_prompt(&mut b, &[0x7f]), PromptEdit::Continue));
        assert_eq!(b, "ws");
        // Enter submits, keeping what was typed so far
        assert!(matches!(edit_prompt(&mut b, b"l\r"), PromptEdit::Submit));
        assert_eq!(b, "wsl");
        // Esc cancels
        let mut c = String::from("pwsh");
        assert!(matches!(edit_prompt(&mut c, &[0x1b]), PromptEdit::Cancel));
    }
}
