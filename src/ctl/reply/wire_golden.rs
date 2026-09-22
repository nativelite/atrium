//! Every reply serializes byte-identically to the builders it replaced.

use super::*;
use crate::ctl::AgentId;

fn entry() -> crate::board::Entry {
    let mut b = crate::board::Board::new();
    b.set(
        "cli",
        &[("status".to_string(), "wip".to_string())],
        Some("lead"),
        1_000,
    );
    let c = b.claim("cli", "dev_1", 5_000, 2_000);
    match c {
        crate::board::Claim::Granted(e) => e,
        _ => panic!("expected grant"),
    }
}

/// The exact bytes the hand-written `reply_*` builders produced before the
/// `Reply` enum (r10 B12), captured by running that code. External consumers
/// parse this JSON; the enum must not change a byte.
fn golden(name: &str) -> &'static str {
    match name {
        "err" => r#"{"ok":false,"err":"no \"pane\" here"}"#,
        "board_entry_some" => r#"{"ok":true,"key":"k","entry":{"a":1}}"#,
        "board_entry_none" => r#"{"ok":true,"key":"k","entry":null}"#,
        "board_list" => r#"{"ok":true,"board":[]}"#,
        "board_del" => r#"{"ok":true,"key":"k","deleted":true}"#,
        "board_claim_granted" => {
            r#"{"ok":true,"key":"cli","granted":true,"holder":"dev_1","lease_ms":7000,"entry":{"by":"dev_1","ms":2000,"claimed_by":"dev_1","lease_ms":7000,"fields":{"status":"wip"}}}"#
        }
        "board_claim_denied" => {
            r#"{"ok":true,"key":"cli","granted":false,"holder":"dev_2","lease_ms":9,"entry":null}"#
        }
        "board_release" => r#"{"ok":true,"key":"k","released":false}"#,
        "bus_published" => {
            r#"{"ok":true,"event":{"seq":4,"topic":"deploy","kind":"fyi"},"subscribers":0}"#
        }
        "bus_subscribed" => r#"{"ok":true,"subscribed":["a","b"]}"#,
        "bus_feed" => r#"{"ok":true,"feed":[],"cursor":12}"#,
        "bus_resolved" => r#"{"ok":true,"seq":7,"resolved":true}"#,
        "bus_topics" => {
            r#"{"ok":true,"topics":[{"topic":"deploy","subs":2},{"topic":"x","subs":0}]}"#
        }
        "spawned" => r#"{"ok":true,"pane":3,"role":"dev_1","session":null,"note":"note"}"#,
        "sent" => r#"{"ok":true,"target":2,"queued":true}"#,
        "killed" => r#"{"ok":true,"killed":[1,4]}"#,
        "audit_some" => r#"{"ok":true,"audit":[{"seq":1}],"oldest_seq":1,"latest_seq":5}"#,
        "audit_none" => r#"{"ok":true,"audit":[],"oldest_seq":null,"latest_seq":0}"#,
        "status_one" => r#"{"ok":true,"pane":2,"status":"working","idle_ms":7000}"#,
        "status_one_none" => r#"{"ok":true,"pane":2,"status":null,"idle_ms":0}"#,
        "list" => {
            r#"{"ok":true,"tree":[{"id":0,"parent":null,"role":"ceo","title":"claude","depth":0,"status":"working","idle_ms":4000},{"id":1,"parent":0,"role":null,"title":"sh","depth":1,"status":null,"idle_ms":0}]}"#
        }
        other => panic!("no golden for {other}"),
    }
}

#[test]
fn every_reply_serializes_byte_identically_to_the_pre_enum_builders() {
    let ev = json::parse(r#"{"seq":4,"topic":"deploy","kind":"fyi"}"#).unwrap();
    let cases: Vec<(&str, String)> = vec![
        ("err", reply_err("no \"pane\" here").to_string()),
        (
            "board_entry_some",
            reply_board_entry("k", Some(json::parse(r#"{"a":1}"#).unwrap())).to_string(),
        ),
        ("board_entry_none", reply_board_entry("k", None).to_string()),
        (
            "board_list",
            reply_board_list(json::parse("[]").unwrap()).to_string(),
        ),
        ("board_del", reply_board_del("k", true).to_string()),
        (
            "board_claim_granted",
            reply_board_claim("cli", &crate::board::Claim::Granted(entry())).to_string(),
        ),
        (
            "board_claim_denied",
            reply_board_claim(
                "cli",
                &crate::board::Claim::Denied {
                    holder: "dev_2".into(),
                    lease_ms: 9,
                },
            )
            .to_string(),
        ),
        ("board_release", reply_board_release("k", false).to_string()),
        (
            "bus_published",
            reply_bus_published(ev.clone(), 0).to_string(),
        ),
        (
            "bus_subscribed",
            reply_bus_subscribed(vec!["a".into(), "b".into()]).to_string(),
        ),
        (
            "bus_feed",
            reply_bus_feed(json::parse("[]").unwrap(), 12).to_string(),
        ),
        ("bus_resolved", reply_bus_resolved(7, true).to_string()),
        (
            "bus_topics",
            reply_bus_topics(vec![("deploy".into(), 2), ("x".into(), 0)]).to_string(),
        ),
        (
            "spawned",
            reply_spawned(AgentId(3), Some("dev_1"), None, Some("note")).to_string(),
        ),
        ("sent", reply_sent(AgentId(2), true).to_string()),
        (
            "killed",
            reply_killed(&[AgentId(1), AgentId(4)]).to_string(),
        ),
        (
            "audit_some",
            reply_audit(vec![json::parse(r#"{"seq":1}"#).unwrap()], Some(1), 5).to_string(),
        ),
        ("audit_none", reply_audit(vec![], None, 0).to_string()),
        (
            "status_one",
            reply_status_one(AgentId(2), Some("working"), 7_000).to_string(),
        ),
        (
            "status_one_none",
            reply_status_one(AgentId(2), None, 0).to_string(),
        ),
        (
            "list",
            reply_list(&[
                TreeNode {
                    id: AgentId(0),
                    parent: None,
                    role: Some("ceo"),
                    title: "claude",
                    depth: 0,
                    status: Some("working"),
                    idle_ms: 4_000,
                },
                TreeNode {
                    id: AgentId(1),
                    parent: Some(AgentId(0)),
                    role: None,
                    title: "sh",
                    depth: 1,
                    status: None,
                    idle_ms: 0,
                },
            ])
            .to_string(),
        ),
    ];
    for (name, json) in cases {
        assert_eq!(json, golden(name), "wire drift in reply {name:?}");
    }
}
