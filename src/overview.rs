use crate::*;

/// One agent in the overview: where it lives (for dive-in), its label, tree
/// depth, live status, identity, and current action.
pub(crate) struct OverviewNode {
    pub(crate) window: usize,
    pub(crate) pane_id: usize,
    pub(crate) label: String,
    pub(crate) depth: usize,
    pub(crate) status: Option<agsess::Status>,
    pub(crate) identity: Option<String>,
    pub(crate) exited: bool,
    pub(crate) action: String,
    /// True if atrium launched this pane as an agent (it has a session id). Lets the
    /// overview show an as-yet-unbound agent as "starting" rather than "shell".
    pub(crate) is_agent: bool,
    /// Short vendor tag from the pane's command stem (`""` for claude, so its
    /// appearance is unchanged; `"gem"`, `"cdx"` for the other supported agents).
    /// Empty for non-agent panes.
    pub(crate) vtag: &'static str,
}

/// Collect every pane, across all windows, as an overview node in spawn-tree
/// order (windows in order, panes in order). Each carries its live agsess status
/// and last action. This is the model the overview renders and the selection
/// cursor indexes into — the same list at 3 agents and 300.
pub(crate) fn overview_nodes(
    windows: &[Window],
    world: &atrium::vendors::VendorWorlds,
) -> Vec<OverviewNode> {
    let mut nodes = Vec::new();
    for (wi, w) in windows.iter().enumerate() {
        for p in &w.panes {
            let status = world.status_for(p.session_id.as_deref());
            let action = p
                .session_id
                .as_deref()
                .and_then(|id| world.sessions().into_iter().find(|s| s.id == id))
                .map(|s| s.last_action.clone())
                .unwrap_or_default();
            nodes.push(OverviewNode {
                window: wi,
                pane_id: p.id,
                label: p.role.clone().unwrap_or_else(|| p.title.clone()),
                depth: p.depth,
                status,
                identity: p.identity.clone(),
                exited: p.exited,
                action,
                is_agent: p.session_id.is_some(),
                vtag: atrium::vendors::vendor_for_stem(&p.title)
                    .map(atrium::vendors::vendor_tag)
                    .unwrap_or(""),
            });
        }
    }
    nodes
}

/// A status → (glyph, SGR color) for an overview node, sharing the bar's color
/// language: green working, amber waiting-on-you, cyan waiting-for-a-message,
/// grey idle, red exited.
pub(crate) fn overview_glyph(node: &OverviewNode) -> (&'static str, String) {
    use atrium::theme;
    let sgr = |c: ansi::Color| {
        ansi::Style {
            fg: c,
            ..Default::default()
        }
        .sgr()
    };
    if node.exited {
        return ("\u{2717}", sgr(theme::EXITED)); // ✗
    }
    match node.status {
        Some(agsess::Status::Working) => ("\u{25CF}", sgr(theme::ACTIVITY)), // ●
        Some(agsess::Status::WaitingApproval) => ("\u{0021}", sgr(theme::WAITING)), // !
        Some(agsess::Status::WaitingPrompt) => ("\u{25D0}", sgr(theme::FOCUSED)), // ◐
        Some(agsess::Status::Idle) => ("\u{00B7}", sgr(theme::IDLE)),        // ·
        None => ("\u{00B7}", sgr(theme::IDLE)),                              // shell / unbound
    }
}

/// A per-window (per-fleet) status rollup for the overview's group header.
pub(crate) struct WindowAgg {
    /// Window index (0-based); rendered 1-based as "window N".
    pub(crate) window: usize,
    pub(crate) total: usize,
    pub(crate) working: usize,
    pub(crate) waiting: usize,
    pub(crate) idle: usize,
    pub(crate) exited: usize,
}

/// One row in the overview body: either an aggregated per-window header or an
/// agent carrying its index into `nodes` (so selection, which indexes `nodes`,
/// maps straight through).
pub(crate) enum OvRow {
    Header(WindowAgg),
    Agent(usize),
}

/// Status rollup for one window across `nodes`.
pub(crate) fn window_agg(nodes: &[OverviewNode], window: usize) -> WindowAgg {
    let mut a = WindowAgg {
        window,
        total: 0,
        working: 0,
        waiting: 0,
        idle: 0,
        exited: 0,
    };
    for n in nodes.iter().filter(|n| n.window == window) {
        a.total += 1;
        if n.exited {
            a.exited += 1;
        } else {
            match n.status {
                Some(agsess::Status::Working) => a.working += 1,
                Some(agsess::Status::WaitingApproval) => a.waiting += 1,
                _ => a.idle += 1,
            }
        }
    }
    a
}

/// Build the overview's display rows. With more than one window present, each
/// window's agents are preceded by a [`WindowAgg`] header so many fleets stay
/// legible at a glance; with a single window the rows are just the agents — the
/// global counts in the panel header already cover that case, and a lone header
/// would be noise. `nodes` are in window order (see [`overview_nodes`]), so a
/// header is emitted whenever the window changes.
pub(crate) fn overview_rows(nodes: &[OverviewNode]) -> Vec<OvRow> {
    let multi = nodes.first().map(|f| f.window).is_some()
        && nodes.iter().any(|n| n.window != nodes[0].window);
    let mut rows = Vec::with_capacity(nodes.len());
    let mut cur: Option<usize> = None;
    for (i, n) in nodes.iter().enumerate() {
        if multi && cur != Some(n.window) {
            cur = Some(n.window);
            rows.push(OvRow::Header(window_agg(nodes, n.window)));
        }
        rows.push(OvRow::Agent(i));
    }
    rows
}

/// The first display row to show so `sel_row` stays visible, filling the viewport
/// (no wasted blank rows when scrolled to the end). Pins the selection near the
/// bottom edge while scrolling down, like the pre-aggregation list did.
pub(crate) fn overview_scroll_start(total: usize, sel_row: usize, list_rows: usize) -> usize {
    if list_rows == 0 || total <= list_rows {
        return 0;
    }
    let start = sel_row.saturating_sub(list_rows.saturating_sub(1));
    start.min(total - list_rows)
}

/// Render the full-screen **overview** overlay (`Ctrl+A o`): a header of live
/// counts, any open decisions, then the agent tree colored by status with a
/// selection cursor (`sel`). With multiple fleets open, each window's agents are
/// grouped under an aggregated header. Diving into the selected agent (Enter) is
/// handled by the caller. Absolute CUP per line; the bar row is left for the bar.
pub(crate) fn render_overview_panel(
    windows: &[Window],
    bus: &atrium::bus::Bus,
    nodes: &[OverviewNode],
    sel: usize,
    rows: u16,
    cols: u16,
) -> String {
    // Redraw in place (no full-screen `2J`) so moving the cursor only changes the
    // rows that changed — no flicker. Every content row ends with `\x1b[K` (clear
    // to EOL), and the rows between the list and the footer are blanked, so a
    // deselected row's highlight and any stale content are wiped without a clear.
    let mut out = String::from("\x1b[?25l");
    let _ = windows; // reserved for future tree connectors
    let (mut working, mut waiting, mut idle, mut exited) = (0, 0, 0, 0);
    for n in nodes {
        if n.exited {
            exited += 1;
        } else {
            match n.status {
                Some(agsess::Status::Working) => working += 1,
                Some(agsess::Status::WaitingApproval) => waiting += 1,
                Some(agsess::Status::WaitingPrompt) | Some(agsess::Status::Idle) | None => {
                    idle += 1
                }
            }
        }
    }
    let decisions = bus.pending_decisions();
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1;38;5;37m  overview\x1b[0m  \x1b[2m{} agents\x1b[0m   \
         \x1b[38;2;95;240;140m\u{25CF} {working} working\x1b[0m   \
         \x1b[38;2;255;200;70m\u{0021} {waiting} waiting\x1b[0m   \
         \x1b[38;2;150;152;165m\u{00B7} {idle} idle\x1b[0m   \
         \x1b[38;2;255;95;95m\u{2717} {exited} exited\x1b[0m{}\x1b[0m\x1b[K",
        nodes.len(),
        if decisions.is_empty() {
            String::new()
        } else {
            format!(
                "   \x1b[1;38;5;11m\u{26A0} {} decisions\x1b[0m",
                decisions.len()
            )
        }
    ));
    out.push_str(&format!(
        "\x1b[2;1H\x1b[38;5;238m{}\x1b[0m",
        "\u{2500}".repeat(cols as usize)
    ));

    let mut row = 3u16;
    let footer_row = rows.saturating_sub(1);

    // Open decisions first — the thing that needs the human.
    if !decisions.is_empty() {
        for e in decisions.iter().take(3) {
            if row >= footer_row {
                break;
            }
            let from = e.from.as_deref().unwrap_or("?");
            let summary = e
                .fields
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!(
                "\x1b[{row};1H  \x1b[1;38;5;11m\u{0021}\x1b[0m \x1b[1m{}\x1b[0m  {summary}  \x1b[2m(from {from})\x1b[0m\x1b[K",
                e.topic
            ));
            row += 1;
        }
        out.push_str(&format!(
            "\x1b[{row};1H\x1b[38;5;238m{}\x1b[0m",
            "\u{2500}".repeat(cols as usize)
        ));
        row += 1;
    }

    // Agent rows (grouped by window when multiple fleets are open), scrolled so
    // the selection stays visible.
    let list_first = row;
    let list_rows = footer_row.saturating_sub(list_first) as usize;
    let mut last_row = row.saturating_sub(1);
    let disp = overview_rows(nodes);
    if disp.is_empty() {
        out.push_str(&format!("\x1b[{row};1H  \x1b[2m(no agents)\x1b[0m\x1b[K"));
        last_row = row;
    } else if list_rows > 0 {
        // Scroll over *display* rows (headers + agents), centering the selected
        // agent (which indexes `nodes`) via its display-row position.
        let sel_row = disp
            .iter()
            .position(|d| matches!(d, OvRow::Agent(i) if *i == sel))
            .unwrap_or(0);
        let start = overview_scroll_start(disp.len(), sel_row, list_rows);
        for (off, d) in disp.iter().enumerate().skip(start).take(list_rows) {
            let r = list_first + (off - start) as u16;
            match d {
                OvRow::Header(a) => {
                    // Aggregated per-window group header: window label + rollup,
                    // reusing the panel-header color scheme. Waiting is highlighted
                    // (it's the count that wants the human).
                    let line = format!(
                        "  \x1b[1;38;5;39m\u{25B8} window {}\x1b[0m  \x1b[2m{} agents\x1b[0m  \
                         \x1b[38;2;95;240;140m\u{25CF}{}\x1b[0m \
                         \x1b[38;2;255;200;70m\u{0021}{}\x1b[0m \
                         \x1b[38;2;150;152;165m\u{00B7}{}\x1b[0m \
                         \x1b[38;2;255;95;95m\u{2717}{}\x1b[0m",
                        a.window + 1,
                        a.total,
                        a.working,
                        a.waiting,
                        a.idle,
                        a.exited,
                    );
                    out.push_str(&format!("\x1b[{r};1H{line}\x1b[0m\x1b[K"));
                }
                OvRow::Agent(i) => {
                    let n = &nodes[*i];
                    let (glyph, color) = overview_glyph(n);
                    let selected = *i == sel;
                    let indent = "  ".repeat(n.depth);
                    let status = n.status.map(status_label).unwrap_or(if n.exited {
                        "exited"
                    } else if n.is_agent {
                        "starting"
                    } else {
                        "shell"
                    });
                    let ident = n
                        .identity
                        .as_deref()
                        .map(|x| format!("  \x1b[2m\u{00B7}{x}\x1b[0m"))
                        .unwrap_or_default();
                    let action = if n.action.is_empty() {
                        String::new()
                    } else {
                        format!("  \x1b[2m\u{2014} {}\x1b[0m", truncate(&n.action, 60))
                    };
                    let cursor = if selected {
                        "\x1b[1;38;5;37m\u{25B8}\x1b[0m"
                    } else {
                        " "
                    };
                    // Vendor tag next to the status (claude's tag is "" → identical).
                    let vtag_str = if n.vtag.is_empty() {
                        String::new()
                    } else {
                        format!(" \x1b[2m{}\x1b[0m", n.vtag)
                    };
                    let line = format!(
                        "{cursor} {indent}{color}{glyph}\x1b[0m \x1b[1m{:<16}\x1b[0m \x1b[2m{status}\x1b[0m{vtag_str}{ident}{action}",
                        truncate(&n.label, 16),
                    );
                    if selected {
                        // Faint bar background; `\x1b[K` fills the row, then the text.
                        out.push_str(&format!("\x1b[{r};1H\x1b[48;5;236m\x1b[K{line}\x1b[0m"));
                    } else {
                        // Reset + `\x1b[K` clears any leftover highlight so nothing
                        // lingers as the cursor moves.
                        out.push_str(&format!("\x1b[{r};1H{line}\x1b[0m\x1b[K"));
                    }
                }
            }
            last_row = r;
        }
    }

    // Blank the rows between the content and the footer (in place, no full clear).
    let mut r = last_row + 1;
    while r < footer_row {
        out.push_str(&format!("\x1b[{r};1H\x1b[K"));
        r += 1;
    }

    out.push_str(&format!(
        "\x1b[{footer_row};1H\x1b[2m  j/k or \u{2191}\u{2193} move  \u{00b7}  Enter dive in  \u{00b7}  Esc / Ctrl+A o close\x1b[0m\x1b[K"
    ));
    out
}
