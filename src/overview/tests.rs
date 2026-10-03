//! The overview panel: per-window aggregates, rows and scrolling.

use super::*;
use crate::test_support::strip_csi;

/// A minimal OverviewNode for aggregation tests.
fn ov_node(window: usize, status: Option<agsess::Status>, exited: bool) -> OverviewNode {
    OverviewNode {
        window,
        pane_id: 0,
        label: "a".into(),
        depth: 0,
        status,
        identity: None,
        exited,
        action: String::new(),
        is_agent: true,
        vtag: "",
        errored: false,
        context_pct: None,
        cost_usd: None,
    }
}

/// An errored agent that also reported its usage.
fn errored_node(window: usize, ctx: u8, cost: f64) -> OverviewNode {
    OverviewNode {
        errored: true,
        context_pct: Some(ctx),
        cost_usd: Some(cost),
        ..ov_node(window, Some(agsess::Status::WaitingPrompt), false)
    }
}

/// An errored agent is counted apart from waiting and idle, its cost joins
/// the window's sum, and exited still wins over everything.
#[test]
fn window_agg_counts_errored_apart_and_sums_cost() {
    let nodes = vec![
        errored_node(0, 40, 1.25),
        OverviewNode {
            cost_usd: Some(0.5),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
        OverviewNode {
            errored: true,
            ..ov_node(0, Some(agsess::Status::Working), true)
        },
    ];
    let a = window_agg(&nodes, 0);
    assert_eq!(
        (a.total, a.working, a.waiting, a.idle, a.exited, a.errored),
        (3, 1, 0, 0, 1, 1)
    );
    assert!((a.cost_usd - 1.75).abs() < 1e-9, "{}", a.cost_usd);
    let (glyph, _) = overview_glyph(&nodes[0]);
    assert_eq!(glyph, "\u{203C}");
    let (glyph, _) = overview_glyph(&nodes[2]);
    assert_eq!(glyph, "\u{2717}", "exited outranks errored");
}

/// The panel says `errored` in its header and on the row, and shows the
/// context fill and cost the mod reported, per agent and in total.
#[test]
fn overview_panel_shows_errored_context_and_cost() {
    let bus = atrium::bus::Bus::new();
    let nodes = vec![
        errored_node(0, 41, 0.12),
        OverviewNode {
            context_pct: Some(7),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
    ];
    let r = strip_csi(&render_overview_panel(&[], &bus, &nodes, 0, 24, 120));
    assert!(r.contains("1 errored"), "{r}");
    assert!(r.contains("$0.12"), "{r}");
    assert!(r.contains("ctx 41% $0.12"), "{r}");
    assert!(r.contains("ctx 7%"), "{r}");
    let plain = vec![ov_node(0, Some(agsess::Status::Working), false)];
    let r = strip_csi(&render_overview_panel(&[], &bus, &plain, 0, 24, 120));
    assert!(
        !r.contains("errored") && !r.contains("ctx ") && !r.contains('$'),
        "{r}"
    );
}

#[test]
fn window_agg_counts_by_status() {
    use agsess::Status::*;
    let nodes = vec![
        ov_node(0, Some(Working), false),
        ov_node(0, Some(WaitingApproval), false),
        ov_node(0, Some(Idle), false),
        ov_node(0, None, false),          // unbound → idle bucket
        ov_node(0, Some(Working), true),  // exited wins over status
        ov_node(1, Some(Working), false), // other window — excluded
    ];
    let a = window_agg(&nodes, 0);
    assert_eq!(a.total, 5);
    assert_eq!(a.working, 1);
    assert_eq!(a.waiting, 1);
    assert_eq!(a.idle, 2);
    assert_eq!(a.exited, 1);
}

#[test]
fn overview_rows_single_window_has_no_header() {
    let nodes = vec![ov_node(0, None, false), ov_node(0, None, false)];
    let rows = overview_rows(&nodes);
    // Two agents, no group header (global counts already cover one fleet).
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| matches!(r, OvRow::Agent(_))));
}

#[test]
fn overview_rows_multi_window_inserts_one_header_per_window() {
    let nodes = vec![
        ov_node(0, None, false),
        ov_node(0, None, false),
        ov_node(1, None, false),
    ];
    let rows = overview_rows(&nodes);
    // Header(w0), Agent0, Agent1, Header(w1), Agent2 = 5 rows.
    assert_eq!(rows.len(), 5);
    let headers: Vec<usize> = rows
        .iter()
        .filter_map(|r| match r {
            OvRow::Header(a) => Some(a.window),
            _ => None,
        })
        .collect();
    assert_eq!(headers, vec![0, 1], "one header per window, in order");
    // Agent rows still carry their original node indices, in order.
    let agents: Vec<usize> = rows
        .iter()
        .filter_map(|r| match r {
            OvRow::Agent(i) => Some(*i),
            _ => None,
        })
        .collect();
    assert_eq!(agents, vec![0, 1, 2]);
}

#[test]
fn overview_scroll_start_keeps_selection_visible_and_fills() {
    // Fits entirely → no scroll.
    assert_eq!(overview_scroll_start(5, 4, 10), 0);
    // Selection near the top → start at 0.
    assert_eq!(overview_scroll_start(20, 2, 5), 0);
    // Selection deep → pinned to the bottom edge (sel at last visible row).
    assert_eq!(overview_scroll_start(20, 12, 5), 8);
    // Selection at the very end → clamp so the viewport is filled, not overshot.
    assert_eq!(overview_scroll_start(20, 19, 5), 15);
    // Degenerate list_rows.
    assert_eq!(overview_scroll_start(20, 19, 0), 0);
}

#[test]
fn overview_panel_shows_window_headers_only_when_multiple_fleets() {
    let bus = atrium::bus::Bus::new();
    // One window: no "window 1" group header.
    let one = vec![ov_node(0, None, false), ov_node(0, None, false)];
    let r1 = strip_csi(&render_overview_panel(&[], &bus, &one, 0, 24, 100));
    assert!(
        !r1.contains("window 1"),
        "single fleet must not show a group header"
    );
    // Two windows: both group headers appear.
    let two = vec![ov_node(0, None, false), ov_node(1, None, false)];
    let r2 = strip_csi(&render_overview_panel(&[], &bus, &two, 0, 24, 100));
    assert!(r2.contains("window 1"), "multi-fleet must group window 1");
    assert!(r2.contains("window 2"), "multi-fleet must group window 2");
}
