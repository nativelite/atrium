use crate::*;

/// Render the full-screen coordination overlay (`Ctrl+A b`): the shared **board**
/// (durable "what is true") on top, a divider, then the **bus** feed (recent
/// events + any open `decision_needed` escalations, "what just happened") below.
/// Hides the cursor, clears, and positions every line with absolute CUP (no
/// scrolling); the bar row is left for the status bar.
pub(crate) fn render_board_panel(
    board: &atrium::board::Board,
    bus: &atrium::bus::Bus,
    rows: u16,
    cols: u16,
    feed_scroll: usize,
    decision_sel: usize,
) -> String {
    let mut out = String::from("\x1b[?25l\x1b[2J");
    let decisions = bus.pending_decisions();
    let has_detail = !decisions.is_empty();
    let sel = decision_sel.min(decisions.len().saturating_sub(1));
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1;38;5;37m  board + bus\x1b[0m  \x1b[2m(Ctrl+A b to close · live)\x1b[0m{}",
        if has_detail {
            format!(
                "  \x1b[1;38;5;11m{} decision{} awaiting you\x1b[0m  \x1b[2m· j/k select · r resolve · g go to agent\x1b[0m",
                decisions.len(),
                if decisions.len() == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        }
    ));
    out.push_str(&format!(
        "\x1b[2;1H\x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat(cols as usize)
    ));

    // The bar lives on `rows`. When a decision is selected, reserve the bottom
    // four rows for a detail bar (divider + 3 content rows) that shows its full,
    // wrapped question — so a truncated one-liner never hides what's being asked.
    let content_last = rows.saturating_sub(1);
    let usable_last = if has_detail {
        content_last.saturating_sub(4).max(3)
    } else {
        content_last
    };

    // Split the usable rows (3..=usable_last) between the board (top) and the
    // bus region (bottom). Defensive on tiny terminals: sections shrink.
    let region = usable_last.saturating_sub(2);
    let feed_h = (region / 2).clamp(3.min(region), region);
    let board_last = usable_last.saturating_sub(feed_h + 1).max(3);

    render_board_rows(&mut out, board, board_last);

    // The bus divider, then the open decisions (selectable), then the scrollable
    // FYI history below them.
    let div_row = board_last + 1;
    out.push_str(&format!(
        "\x1b[{div_row};1H\x1b[38;5;238m\u{2500}\u{2500} \x1b[0m\x1b[1;38;5;37mbus\x1b[0m \x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat((cols as usize).saturating_sub(7))
    ));
    let after_decisions =
        render_decisions_block(&mut out, &decisions, sel, div_row + 1, usable_last);
    render_fyi_feed(
        &mut out,
        bus,
        &decisions,
        after_decisions,
        usable_last,
        feed_scroll,
    );

    if has_detail {
        render_decision_detail(&mut out, decisions.get(sel), content_last, cols);
    }
    out
}

/// Draw the open decisions as a selectable list from `first_row` down, the
/// selected one marked with a bright `▶` and bolded. Returns the next free row
/// (where the FYI feed begins). Decisions past the space are dropped — the detail
/// bar still shows the selected one in full.
pub(crate) fn render_decisions_block(
    out: &mut String,
    decisions: &[&atrium::bus::Event],
    sel: usize,
    first_row: u16,
    last_row: u16,
) -> u16 {
    let mut row = first_row;
    for (i, d) in decisions.iter().enumerate() {
        if row > last_row {
            break;
        }
        let body = feed_line(d);
        let body = body.trim_start(); // drop the leading indent; we add our own marker
        if i == sel {
            out.push_str(&format!(
                "\x1b[{row};1H \x1b[1;38;5;11m\u{25B6}\x1b[0m \x1b[1m{body}\x1b[0m"
            ));
        } else {
            out.push_str(&format!("\x1b[{row};1H   {body}"));
        }
        row += 1;
    }
    row
}

/// The detail bar for the selected decision: a divider, then its seq + source
/// persona, then the full question wrapped across up to two rows — so a long
/// question that truncates in the list is always fully readable here.
pub(crate) fn render_decision_detail(
    out: &mut String,
    decision: Option<&&atrium::bus::Event>,
    last_row: u16,
    cols: u16,
) {
    let div_row = last_row.saturating_sub(3);
    out.push_str(&format!(
        "\x1b[{div_row};1H\x1b[38;5;238m\u{2500}\u{2500} \x1b[0m\x1b[1;38;5;11mselected decision\x1b[0m \x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat((cols as usize).saturating_sub(20))
    ));
    let Some(d) = decision else { return };
    let from = d.from.as_deref().unwrap_or("?");
    // Head row: seq + who raised it + the action hints.
    out.push_str(&format!(
        "\x1b[{};1H  \x1b[1;38;5;11m#{}\x1b[0m from \x1b[1m{from}\x1b[0m  \x1b[2m·  r resolve  ·  g go to {from}  ·  esc close\x1b[0m",
        div_row + 1,
        d.seq
    ));
    // The full question, preferring a `q=` field, else all fields joined, wrapped
    // across the two remaining rows (truncated with … only if it overflows both).
    let question = d
        .fields
        .iter()
        .find(|(k, _)| k.as_str() == "q")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            d.fields
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("  ")
        });
    let width = (cols as usize).saturating_sub(4).max(8);
    for (j, line) in wrap_to(&question, width, 2).into_iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H  {line}", div_row + 2 + j as u16));
    }
}

/// Word-wrap `text` to `width` columns across at most `max_lines` lines; if it
/// still overflows, the last line ends with `…`. Whitespace-collapsing — good
/// enough for a one-shot question, not a general typesetter.
// `while let … next()` (not `for`) is deliberate: after the loop `words` is
// reused via `words.peek()` to detect overflow, which a `for` would consume.
#[allow(clippy::while_let_on_iterator)]
pub(crate) fn wrap_to(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut words = text.split_whitespace().peekable();
    while let Some(w) = words.next() {
        if cur.is_empty() {
            cur = w.to_string();
        } else if cur.chars().count() + 1 + w.chars().count() <= width {
            cur.push(' ');
            cur.push_str(w);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = w.to_string();
            if lines.len() == max_lines {
                break;
            }
        }
    }
    if lines.len() < max_lines && !cur.is_empty() {
        lines.push(cur);
    }
    // Overflow: more words remain than fit → mark the last line truncated.
    if words.peek().is_some() {
        if let Some(last) = lines.last_mut() {
            let keep = width.saturating_sub(1);
            let trimmed: String = last.chars().take(keep).collect();
            *last = format!("{trimmed}…");
        }
    }
    lines
}

/// One row of the merged activity log: a timestamp, a colored source glyph, who,
/// and what.
pub(crate) struct LogRow {
    pub(crate) ts: u64,
    pub(crate) glyph: &'static str,
    pub(crate) who: String,
    pub(crate) text: String,
}

/// Merge the bus, the board, and each agent's latest action into one activity
/// log, sorted oldest→newest. The bus is the bulk (every event, already stamped
/// and attributed); the board contributes each current entry at its last-update
/// time; agsess contributes each agent's most recent action, attributed to the
/// persona via the pane that owns its session.
pub(crate) fn collect_log(
    windows: &[Window],
    world: &atrium::vendors::VendorWorlds,
    board: &atrium::board::Board,
    bus: &atrium::bus::Bus,
) -> Vec<LogRow> {
    let mut rows: Vec<LogRow> = Vec::new();
    for e in bus.tail(atrium::bus::RING_CAP) {
        let glyph = match e.kind {
            atrium::bus::Kind::DecisionNeeded => "\x1b[1;38;5;11m!\x1b[0m",
            atrium::bus::Kind::Fyi => "\x1b[38;5;37m\u{00B7}\x1b[0m",
        };
        let fields = e
            .fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        rows.push(LogRow {
            ts: e.ts_ms,
            glyph,
            who: e.from.clone().unwrap_or_else(|| "?".to_string()),
            text: format!("{}  {}", e.topic, fields),
        });
    }
    for (key, entry) in board.list() {
        let status = entry.fields.get("status").cloned().unwrap_or_default();
        rows.push(LogRow {
            ts: entry.updated_ms,
            glyph: "\x1b[38;5;10m\u{25C6}\x1b[0m",
            who: entry.updated_by.clone().unwrap_or_default(),
            text: format!("board {key} = {status}"),
        });
    }
    for s in world.sessions() {
        if let Some(ts) = s.last_ts_ms {
            if s.last_action.is_empty() {
                continue;
            }
            let who = windows
                .iter()
                .flat_map(|w| &w.panes)
                .find(|p| p.session_id.as_deref() == Some(s.id.as_str()))
                .and_then(|p| p.role.clone())
                .unwrap_or_else(|| format!("session {}", &s.id[..s.id.len().min(6)]));
            rows.push(LogRow {
                ts,
                glyph: "\x1b[38;5;39m\u{25B8}\x1b[0m",
                who,
                text: s.last_action.clone(),
            });
        }
    }
    rows.sort_by_key(|r| r.ts);
    rows
}

/// Compact "time ago" for a log stamp — timezone-free and zero-dep: `12s`, `3m`,
/// `2h`, `4d`.
pub(crate) fn ago(now_ms: u64, ts_ms: u64) -> String {
    let secs = now_ms.saturating_sub(ts_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// The activity-log panel (`Ctrl+A l`): the merged [`collect_log`] rendered
/// newest-at-the-bottom (tailing), scrollable up for history. `scroll` is how
/// many events back from the newest the window is shifted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_log_panel(
    windows: &[Window],
    world: &atrium::vendors::VendorWorlds,
    board: &atrium::board::Board,
    bus: &atrium::bus::Bus,
    rows: u16,
    cols: u16,
    scroll: usize,
    now_ms: u64,
) -> String {
    let mut out = String::from("\x1b[?25l\x1b[2J");
    let log = collect_log(windows, world, board, bus);
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1;38;5;37m  activity log\x1b[0m  \x1b[2m(Ctrl+A l to close · live · {} events · PgUp/PgDn scroll)\x1b[0m",
        log.len()
    ));
    out.push_str(&format!(
        "\x1b[2;1H\x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat(cols as usize)
    ));
    let content_last = rows.saturating_sub(1);
    let first_row = 3u16;
    if first_row > content_last {
        return out;
    }
    if log.is_empty() {
        out.push_str(&format!(
            "\x1b[{first_row};1H  \x1b[2m(nothing yet — the bus, board, and agent actions land here)\x1b[0m"
        ));
        return out;
    }
    let mut cap = (content_last - first_row + 1) as usize;
    // Reserve the bottom row for a scroll hint when the log overflows the panel.
    let scrollable = log.len() > cap;
    let hint = if scrollable && cap > 0 {
        cap -= 1;
        let scroll = scroll.min(log.len() - cap);
        let older = log.len() - cap - scroll;
        Some(format!(
            "\x1b[2m  \u{2195} PgUp/PgDn · {older} older ↑ · {scroll} newer ↓\x1b[0m"
        ))
    } else {
        None
    };
    // Newest at the bottom: the window ends `scroll` events before the newest.
    let scroll = scroll.min(log.len().saturating_sub(cap));
    let start = log.len().saturating_sub(cap).saturating_sub(scroll);
    for (i, r) in log[start..(start + cap).min(log.len())].iter().enumerate() {
        let row = first_row + i as u16;
        let stamp = format!("{:>4}", ago(now_ms, r.ts));
        let who: String = r.who.chars().take(16).collect();
        // Budget the variable text so the visible line fits `cols` (the prefix is
        // "  " + stamp(4) + " " + glyph(1) + " " + who + " · ").
        let head = 2 + 4 + 1 + 1 + 1 + who.chars().count() + 3;
        let budget = (cols as usize).saturating_sub(head).max(4);
        let text: String = r.text.chars().take(budget).collect();
        out.push_str(&format!(
            "\x1b[{row};1H  \x1b[2m{stamp}\x1b[0m {} \x1b[1m{who}\x1b[0m \x1b[2m·\x1b[0m {text}",
            r.glyph
        ));
    }
    if let Some(h) = hint {
        out.push_str(&format!("\x1b[{content_last};1H\x1b[K{h}"));
    }
    out
}

/// Draw the board entries into `out`, from row 3 down to `last_row` (inclusive),
/// with an overflow hint if there are more than fit.
pub(crate) fn render_board_rows(out: &mut String, board: &atrium::board::Board, last_row: u16) {
    use atrium::ctl::{hyperlink, is_url, status_glyph, status_sgr};
    let entries = board.list();
    if entries.is_empty() {
        out.push_str(
            "\x1b[3;1H  \x1b[2m(board empty — set one:  atrium ctl board set launch status=WIP owner=you)\x1b[0m",
        );
        return;
    }
    let key_w = entries
        .iter()
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 24);
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
        let status_label = status
            .map(|st| format!("{color}{st}\x1b[0m  "))
            .unwrap_or_default();
        let mut fields = String::new();
        for (f, v) in &e.fields {
            if f == "status" {
                continue;
            }
            let rendered = if is_url(v) {
                hyperlink(v, v)
            } else {
                v.clone()
            };
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

/// Draw the scrollable FYI history into `out`, from `first_row` down to
/// `last_row`, newest first (dim `·`), excluding the open decisions shown in
/// their own block above. URLs render as clickable OSC-8 links; a bottom-row
/// hint shows the scroll offset when the history overflows.
pub(crate) fn render_fyi_feed(
    out: &mut String,
    bus: &atrium::bus::Bus,
    decisions: &[&atrium::bus::Event],
    first_row: u16,
    last_row: u16,
    scroll: usize,
) {
    if first_row > last_row {
        return;
    }
    let mut cap = (last_row - first_row + 1) as usize;
    let open_seqs: std::collections::BTreeSet<u64> = decisions.iter().map(|e| e.seq).collect();

    // The scrollable FYI history: every non-decision event, newest first (open
    // decisions are shown in their own block above and excluded here).
    let fyi: Vec<String> = bus
        .tail(atrium::bus::RING_CAP)
        .into_iter()
        .rev()
        .filter(|e| !open_seqs.contains(&e.seq))
        .map(feed_line)
        .collect();

    // When the history overflows the space, reserve the bottom row for a scroll
    // hint so the offset is legible and the affordance is discoverable.
    let scrollable = fyi.len() > cap;
    let hint = if scrollable && cap > 0 {
        cap -= 1;
        let start = scroll.min(fyi.len().saturating_sub(cap));
        let newer = start;
        let older = fyi.len().saturating_sub(start + cap);
        Some(format!(
            "\x1b[2m  \u{2195} PgUp/PgDn scroll · {newer} newer · {older} older\x1b[0m"
        ))
    } else {
        None
    };

    // Window into the FYI history at the (clamped) scroll offset.
    let start = if fyi.len() > cap {
        scroll.min(fyi.len() - cap)
    } else {
        0
    };
    let shown: Vec<String> = fyi.into_iter().skip(start).take(cap).collect();

    if shown.is_empty() && decisions.is_empty() {
        out.push_str(&format!(
            "\x1b[{first_row};1H  \x1b[2m(no events — publish one:  atrium ctl bus pub deploy msg=shipping)\x1b[0m"
        ));
        return;
    }
    for (i, line) in shown.into_iter().enumerate() {
        let row = first_row + i as u16;
        out.push_str(&format!("\x1b[{row};1H{line}"));
    }
    if let Some(h) = hint {
        out.push_str(&format!("\x1b[{last_row};1H\x1b[K{h}"));
    }
}

/// One bus event as a panel line: `! deploy  msg=ship it?  (from dev_1)`. A
/// `decision_needed` event gets an amber `!` and bold topic; an FYI a dim `·`.
pub(crate) fn feed_line(e: &atrium::bus::Event) -> String {
    use atrium::bus::Kind;
    use atrium::ctl::{hyperlink, is_url};
    let (glyph, topic_sgr) = match e.kind {
        Kind::DecisionNeeded => ("\x1b[1;38;5;11m!\x1b[0m", "\x1b[1;38;5;11m"),
        Kind::Fyi => ("\x1b[2m·\x1b[0m", "\x1b[1m"),
    };
    let fields = e
        .fields
        .iter()
        .map(|(f, v)| {
            let rendered = if is_url(v) {
                hyperlink(v, v)
            } else {
                v.clone()
            };
            format!("\x1b[2m{f}=\x1b[0m{rendered}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    let from = e
        .from
        .as_deref()
        .map(|f| format!("  \x1b[2m(from {f})\x1b[0m"))
        .unwrap_or_default();
    // A decision shows its seq so it can be cleared with `bus resolve <seq>`
    // after you answer it in the agent's pane; FYIs carry no actionable id.
    let seq_tag = match e.kind {
        Kind::DecisionNeeded => format!("\x1b[38;5;11m#{}\x1b[0m ", e.seq),
        Kind::Fyi => String::new(),
    };
    format!(
        "  {glyph} {seq_tag}{topic_sgr}{}\x1b[0m  {fields}{from}",
        e.topic
    )
}

pub(crate) fn draw_startup_splash(
    out: &mut impl std::io::Write,
    rows: u16,
    cols: u16,
    frame: usize,
) {
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
        let (r, g, b) = atrium::theme::SPLASH_GRADIENT[i];
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
