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
        agent_id: 0,
        doing: None,
        colliding: false,
        leads_team: None,
    }
}

/// A live Claude Code team as `atrium::claude_teams` would snapshot it: a lead
/// and two teammates, one task in each state plus a blocked one.
fn team_fixture() -> atrium::claude_teams::TeamState {
    use atrium::claude_teams::{Member, Task, TaskStatus, TeamState};
    let task = |id: &str, subject: &str, status: TaskStatus, owner: Option<&str>| Task {
        id: id.into(),
        subject: subject.into(),
        status,
        owner: owner.map(str::to_string),
        blocked_by: Vec::new(),
        active_form: None,
    };
    TeamState {
        name: "session-9e251086".into(),
        lead_agent_id: Some("team-lead@session-9e251086".into()),
        members: ["team-lead", "builder", "writer"]
            .iter()
            .map(|n| Member {
                name: n.to_string(),
                agent_id: format!("{n}@session-9e251086"),
            })
            .collect(),
        tasks: vec![
            task(
                "1",
                "Parse the team config",
                TaskStatus::InProgress,
                Some("builder"),
            ),
            Task {
                blocked_by: vec!["1".into()],
                ..task("2", "Render the block", TaskStatus::Pending, None)
            },
            task("3", "Write the docs", TaskStatus::Completed, Some("writer")),
        ],
    }
}

/// Under the lead's row the overview lists its Claude Code team: a summary
/// line, then one line per task with its mark, owner and what blocks it; a
/// teammate without a mod caption is captioned with its current task's subject;
/// the rows interleave with the agents without disturbing their indices.
#[test]
fn overview_panel_lists_the_lead_team_tasks_under_its_row() {
    let bus = atrium::bus::Bus::new();
    let teams = vec![team_fixture()];
    let nodes = vec![
        OverviewNode {
            label: "lead".into(),
            agent_id: 1,
            leads_team: Some("session-9e251086".into()),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
        OverviewNode {
            label: "builder".into(),
            agent_id: 2,
            depth: 1,
            doing: team_caption("builder", Some("session-9e251086"), &teams),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
        OverviewNode {
            label: "writer".into(),
            agent_id: 3,
            depth: 1,
            doing: team_caption("writer", Some("session-9e251086"), &teams),
            action: "Bash cargo test".into(),
            ..ov_node(0, Some(agsess::Status::Idle), false)
        },
    ];
    let rows = overview_rows_with(&nodes, &teams);
    // Agent0, Team summary, 3 tasks, Agent1, Agent2.
    assert_eq!(
        rows.len(),
        7,
        "one summary and three task rows under the lead"
    );
    assert!(matches!(
        rows[1],
        OvRow::Team {
            team: 0,
            task: None
        }
    ));
    assert!(matches!(
        rows[4],
        OvRow::Team {
            team: 0,
            task: Some(2)
        }
    ));
    assert!(matches!(rows[5], OvRow::Agent(1)));
    // Without teams the rows are the agents alone (the lead still names one).
    assert_eq!(overview_rows_with(&nodes, &[]).len(), 3);

    let r = strip_csi(&render_overview_panel_with(
        &[],
        &bus,
        &nodes,
        &[],
        &teams,
        0,
        24,
        120,
    ));
    assert!(
        r.contains("team session-9e251086  3 tasks  [ ]1 [~]1 [x]1"),
        "summary line: {r}"
    );
    assert!(r.contains("[~] Parse the team config  (builder)"), "{r}");
    assert!(
        r.contains("[ ] Render the block  blocked by 1"),
        "a pending task names what blocks it: {r}"
    );
    assert!(r.contains("[x] Write the docs  (writer)"), "{r}");
    assert!(
        r.contains("builder          working  — Parse the team config"),
        "the teammate's caption is its task: {r}"
    );
    assert!(
        r.contains("— Bash cargo test"),
        "a teammate whose task is completed keeps the transcript's last action: {r}"
    );
    let lead_at = r.find("lead").unwrap();
    let team_at = r.find("team session-").unwrap();
    let builder_at = r.find("builder ").unwrap();
    assert!(
        lead_at < team_at && team_at < builder_at,
        "block sits between lead and teammates"
    );
    // A completed task's owner has no current task; a stranger has none.
    assert_eq!(team_caption("writer", None, &teams), None);
    assert_eq!(team_caption("nobody", None, &teams), None);
    assert_eq!(
        team_caption("builder", Some("session-other"), &teams),
        None,
        "a teammate of another team is not captioned from this one"
    );
    assert_eq!(
        team_caption("builder", None, &teams).as_deref(),
        Some("Parse the team config"),
        "with no --team-name any live team is consulted"
    );
}

/// A collision lists the file and both panes by label, counts in the header,
/// marks each colliding row with a bolt, and the caption replaces the
/// transcript's last action on the row.
#[test]
fn overview_panel_shows_collisions_and_captions() {
    let bus = atrium::bus::Bus::new();
    let nodes = vec![
        OverviewNode {
            label: "dev_1".into(),
            agent_id: 1,
            colliding: true,
            doing: Some("editing filter.rs".into()),
            action: "Read src/old.rs".into(),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
        OverviewNode {
            label: "dev_2".into(),
            agent_id: 2,
            colliding: true,
            action: "Bash cargo test".into(),
            ..ov_node(0, Some(agsess::Status::Working), false)
        },
    ];
    let collisions = vec![atrium::modstate::Collision {
        key: "src/filter.rs".into(),
        panes: vec![1, 2],
    }];
    let r = strip_csi(&render_overview_panel(
        &[],
        &bus,
        &nodes,
        &collisions,
        0,
        24,
        120,
    ));
    assert!(r.contains("1 collision"), "{r}");
    assert!(r.contains("src/filter.rs  edited by dev_1, dev_2"), "{r}");
    assert!(
        r.contains("— editing filter.rs"),
        "the caption is the action: {r}"
    );
    assert!(!r.contains("Read src/old.rs"), "{r}");
    assert!(
        r.contains("— Bash cargo test"),
        "no caption: the last action: {r}"
    );
    assert_eq!(
        r.matches('\u{26A1}').count(),
        4,
        "header, the collision line, a bolt on each row: {r}"
    );
    let r = strip_csi(&render_overview_panel(&[], &bus, &nodes, &[], 0, 24, 120));
    assert!(!r.contains("collision"), "{r}");
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
    let r = strip_csi(&render_overview_panel(&[], &bus, &nodes, &[], 0, 24, 120));
    assert!(r.contains("1 errored"), "{r}");
    assert!(r.contains("$0.12"), "{r}");
    assert!(r.contains("ctx 41% $0.12"), "{r}");
    assert!(r.contains("ctx 7%"), "{r}");
    let plain = vec![ov_node(0, Some(agsess::Status::Working), false)];
    let r = strip_csi(&render_overview_panel(&[], &bus, &plain, &[], 0, 24, 120));
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
    let rows = overview_rows_with(&nodes, &[]);
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
    let rows = overview_rows_with(&nodes, &[]);
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
    let r1 = strip_csi(&render_overview_panel(&[], &bus, &one, &[], 0, 24, 100));
    assert!(
        !r1.contains("window 1"),
        "single fleet must not show a group header"
    );
    // Two windows: both group headers appear.
    let two = vec![ov_node(0, None, false), ov_node(1, None, false)];
    let r2 = strip_csi(&render_overview_panel(&[], &bus, &two, &[], 0, 24, 100));
    assert!(r2.contains("window 1"), "multi-fleet must group window 1");
    assert!(r2.contains("window 2"), "multi-fleet must group window 2");
}
