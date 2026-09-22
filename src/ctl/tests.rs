//! Request round trips (client `build_request` -> server `parse_request`)
//! and the request shapes both halves agree on.

use super::*;
use crate::ctl::testutil::v;

/// `atrium ctl bus topics` round-trips argv → request → parsed `BusOp::Topics`.
#[test]
fn bus_topics_builds_and_parses() {
    let req = build_request(&v(&["bus", "topics"]), None).expect("build");
    match parse_request(&req).expect("parse").cmd {
        Cmd::Bus(BusOp::Topics) => {}
        other => panic!("expected BusOp::Topics, got {other:?}"),
    }
}

/// Existing bus ops still parse — the new op is purely additive (criterion [2]).
#[test]
fn existing_bus_ops_unaffected_by_topics_addition() {
    let req = build_request(&v(&["bus", "feed", "--since", "3"]), None).unwrap();
    assert!(matches!(
        parse_request(&req).unwrap().cmd,
        Cmd::Bus(BusOp::Feed { since: 3 })
    ));
}

#[test]
fn an_unparseable_mode_is_an_error_not_an_absence() {
    // It used to collapse to None — indistinguishable from "not supplied" —
    // so it inherited the session policy and failed OPEN toward the more
    // permissive mode. Only a hand-rolled or hostile request gets here.
    let err = parse_request(r#"{"cmd":"spawn","argv":["claude"],"mode":"definitely-not-a-mode"}"#)
        .unwrap_err();
    assert!(
        err.contains("definitely-not-a-mode"),
        "the error should name the bad mode, got {err:?}"
    );
    // A genuinely absent mode is still fine: it means "inherit the policy".
    assert!(parse_request(r#"{"cmd":"spawn","argv":["claude"]}"#).is_ok());
    // And a valid one still parses.
    assert!(parse_request(r#"{"cmd":"spawn","argv":["claude"],"mode":"plan"}"#).is_ok());
}

#[test]
fn build_spawn_request_roundtrips_through_parse() {
    let line = build_request(&v(&["spawn", "--role", "dev_1", "--", "claude"]), Some(0)).unwrap();
    let req = parse_request(&line).unwrap();
    assert_eq!(req.caller, Some(0));
    match req.cmd {
        Cmd::Spawn(sp) => {
            assert_eq!(sp.role.as_deref(), Some("dev_1"));
            assert_eq!(sp.argv, v(&["claude"]));
            assert!(sp.new_window);
        }
        _ => panic!("expected spawn"),
    }
}

#[test]
fn build_here_sets_window_false() {
    let line = build_request(&v(&["spawn", "--here", "--", "claude"]), None).unwrap();
    let req = parse_request(&line).unwrap();
    match req.cmd {
        Cmd::Spawn(sp) => assert!(!sp.new_window),
        _ => panic!("expected spawn"),
    }
}

/// r10 B10: a field value may itself contain `=` (query strings, base64), and
/// an empty value must survive as empty. Pins `split_once` semantics.
#[test]
fn field_values_keep_embedded_equals_and_empty_values() {
    let line = build_request(
        &v(&[
            "bus",
            "pub",
            "deploy",
            "url=https://x/pr/9?a=b&c=d",
            "blob=YQ==",
            "note=",
        ]),
        Some(0),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Bus(BusOp::Pub { fields, .. }) => {
            let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.as_str());
            assert_eq!(get("url"), Some("https://x/pr/9?a=b&c=d"));
            assert_eq!(get("blob"), Some("YQ=="));
            assert_eq!(get("note"), Some(""));
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn build_bus_pub_roundtrips_through_parse() {
    let line = build_request(
        &v(&[
            "bus",
            "pub",
            "deploy",
            "msg=shipping v2",
            "url=https://x/pr/9",
        ]),
        Some(0),
    )
    .unwrap();
    let req = parse_request(&line).unwrap();
    match req.cmd {
        Cmd::Bus(BusOp::Pub {
            topic,
            kind,
            fields,
            ..
        }) => {
            assert_eq!(topic, "deploy");
            assert_eq!(kind, crate::bus::Kind::Fyi, "defaults to fyi");
            assert!(fields.contains(&("msg".to_string(), "shipping v2".to_string())));
            assert!(fields.contains(&("url".to_string(), "https://x/pr/9".to_string())));
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn build_bus_pub_decision_flag_sets_kind() {
    let line = build_request(
        &v(&["bus", "pub", "release", "--decision", "q=ship now?"]),
        None,
    )
    .unwrap();
    let req = parse_request(&line).unwrap();
    match req.cmd {
        Cmd::Bus(BusOp::Pub { kind, .. }) => {
            assert_eq!(kind, crate::bus::Kind::DecisionNeeded)
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn build_bus_pub_new_flag_sets_create() {
    let with = parse_request(
        &build_request(&v(&["bus", "pub", "adhoc", "--new", "msg=hi"]), None).unwrap(),
    )
    .unwrap();
    match with.cmd {
        Cmd::Bus(BusOp::Pub { create, .. }) => assert!(create, "--new sets create"),
        other => panic!("expected bus pub, got {other:?}"),
    }
    let without =
        parse_request(&build_request(&v(&["bus", "pub", "adhoc", "msg=hi"]), None).unwrap())
            .unwrap();
    match without.cmd {
        Cmd::Bus(BusOp::Pub { create, .. }) => assert!(!create, "default is create=false"),
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn build_bus_sub_and_feed_roundtrip() {
    let sub =
        parse_request(&build_request(&v(&["bus", "sub", "deploy", "*"]), None).unwrap()).unwrap();
    assert_eq!(
        sub.cmd,
        Cmd::Bus(BusOp::Sub {
            topics: v(&["deploy", "*"])
        })
    );
    let feed =
        parse_request(&build_request(&v(&["bus", "feed", "--since", "5"]), None).unwrap()).unwrap();
    assert_eq!(feed.cmd, Cmd::Bus(BusOp::Feed { since: 5 }));
}

#[test]
fn build_bus_resolve_roundtrip() {
    let r = parse_request(&build_request(&v(&["bus", "resolve", "7"]), None).unwrap()).unwrap();
    assert_eq!(r.cmd, Cmd::Bus(BusOp::Resolve { seq: 7 }));
}

#[test]
fn bus_pub_joins_unquoted_spaced_values() {
    // The shell splits `msg=merged the PR` into three tokens; the parser must
    // rejoin the continuation words into one value (no quoting needed).
    let line = build_request(
        &v(&[
            "bus",
            "pub",
            "deploy",
            "msg=merged",
            "the",
            "PR",
            "url=https://x/pr/42",
        ]),
        None,
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Bus(BusOp::Pub { fields, .. }) => {
            assert!(fields.contains(&("msg".to_string(), "merged the PR".to_string())));
            assert!(fields.contains(&("url".to_string(), "https://x/pr/42".to_string())));
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn bus_pub_decision_keeps_spaced_value() {
    let line = build_request(
        &v(&[
            "bus",
            "pub",
            "release",
            "--decision",
            "q=ship",
            "v2",
            "now?",
        ]),
        None,
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Bus(BusOp::Pub { kind, fields, .. }) => {
            assert_eq!(kind, crate::bus::Kind::DecisionNeeded);
            assert!(fields.contains(&("q".to_string(), "ship v2 now?".to_string())));
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
}

#[test]
fn board_set_joins_unquoted_spaced_values() {
    let line = build_request(
        &v(&["board", "set", "task", "status=in", "progress", "owner=Max"]),
        None,
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Board(BoardOp::Set { fields, .. }) => {
            assert!(fields.contains(&("status".to_string(), "in progress".to_string())));
            assert!(fields.contains(&("owner".to_string(), "Max".to_string())));
        }
        other => panic!("expected board set, got {other:?}"),
    }
}

#[test]
fn build_and_parse_send_request() {
    let line = build_request(&v(&["send", "dev_1", "implement", "X", "TDD"]), Some(0)).unwrap();
    let req = parse_request(&line).unwrap();
    assert_eq!(req.caller, Some(0));
    match req.cmd {
        Cmd::Send(sr) => {
            assert_eq!(sr.target, "dev_1");
            assert_eq!(sr.text, "implement X TDD");
        }
        _ => panic!("expected send"),
    }
}

#[test]
fn build_and_parse_status_request_with_and_without_target() {
    let one = parse_request(&build_request(&v(&["status", "3"]), None).unwrap()).unwrap();
    assert_eq!(
        one.cmd,
        Cmd::Status(StatusReq {
            target: Some("3".into())
        })
    );
    let all = parse_request(&build_request(&v(&["status"]), None).unwrap()).unwrap();
    assert_eq!(all.cmd, Cmd::Status(StatusReq { target: None }));
}

#[test]
fn ctl_read_ops_are_open_mutations_require_auth() {
    // Reads are servable to an unauthenticated caller…
    assert!(Cmd::List.is_read_only());
    assert!(Cmd::Board(BoardOp::List).is_read_only());
    assert!(Cmd::Board(BoardOp::Get { key: "auth".into() }).is_read_only());
    assert!(Cmd::Bus(BusOp::Feed { since: 0 }).is_read_only());
    // …while anything that mutates the fleet or shared state is gated.
    assert!(!Cmd::Kill(KillReq { target: "w".into() }).is_read_only());
    assert!(!Cmd::Board(BoardOp::Set {
        key: "t".into(),
        fields: vec![],
    })
    .is_read_only());
    assert!(!Cmd::Board(BoardOp::Claim {
        key: "t".into(),
        ttl_ms: None,
    })
    .is_read_only());
    assert!(!Cmd::Bus(BusOp::Resolve { seq: 1 }).is_read_only());
}

#[test]
fn audit_labels_are_secret_free() {
    // `send` records the text length, never the body.
    let (action, detail) = Cmd::Send(SendReq {
        target: "dev".into(),
        text: "the secret plan".into(),
    })
    .audit_label();
    assert_eq!(action, "send");
    assert_eq!(detail, "target=dev len=15");
}

#[test]
fn build_list_request() {
    let line = build_request(&v(&["list"]), Some(3)).unwrap();
    let req = parse_request(&line).unwrap();
    assert_eq!(req.caller, Some(3));
    assert_eq!(req.cmd, Cmd::List);
}

#[test]
fn parse_rejects_unknown_command() {
    let err = parse_request(r#"{"cmd":"frobnicate"}"#).unwrap_err();
    assert!(err.contains("unknown command"), "{err}");
}

#[test]
fn build_spawn_mode_roundtrips_through_parse() {
    // `ctl spawn --mode automode` reaches the server as SpawnReq.mode = Auto
    // (auto mode) — NOT Skip (full bypass); they are separate.
    let line = build_request(
        &v(&["spawn", "--mode", "automode", "--", "claude"]),
        Some(0),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.mode, Some(TrustMode::Auto)),
        _ => panic!("expected spawn"),
    }
    // `--mode skip` is the separate full-bypass request.
    let sk = build_request(&v(&["spawn", "--mode", "skip", "--", "claude"]), Some(0)).unwrap();
    match parse_request(&sk).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.mode, Some(TrustMode::Skip)),
        _ => panic!("expected spawn"),
    }
    // No --mode ⇒ None (inherit the session policy).
    let plain = build_request(&v(&["spawn", "--", "claude"]), None).unwrap();
    match parse_request(&plain).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.mode, None),
        _ => panic!("expected spawn"),
    }
    // An unknown --mode is a clear client error.
    assert!(
        build_request(&v(&["spawn", "--mode", "yolo", "--", "claude"]), None)
            .unwrap_err()
            .contains("--mode")
    );
}

#[test]
fn build_board_set_roundtrips_through_parse() {
    // `board set auth status=DONE owner=Max` → BoardOp::Set with both fields.
    let line = build_request(
        &v(&["board", "set", "auth", "status=DONE", "owner=Max"]),
        Some(0),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Board(BoardOp::Set { key, fields }) => {
            assert_eq!(key, "auth");
            assert!(fields.contains(&("status".to_string(), "DONE".to_string())));
            assert!(fields.contains(&("owner".to_string(), "Max".to_string())));
        }
        _ => panic!("expected board set"),
    }
}

#[test]
fn build_board_get_list_del_roundtrip() {
    let get = build_request(&v(&["board", "get", "auth"]), None).unwrap();
    assert!(matches!(
        parse_request(&get).unwrap().cmd,
        Cmd::Board(BoardOp::Get { key }) if key == "auth"
    ));
    let list = build_request(&v(&["board", "list"]), None).unwrap();
    assert!(matches!(
        parse_request(&list).unwrap().cmd,
        Cmd::Board(BoardOp::List)
    ));
    let del = build_request(&v(&["board", "del", "auth"]), None).unwrap();
    assert!(matches!(
        parse_request(&del).unwrap().cmd,
        Cmd::Board(BoardOp::Del { key }) if key == "auth"
    ));
}

#[test]
fn bus_pub_to_addresses_a_decision_to_a_role() {
    // `--to lead` becomes a `to=lead` field, so the server/surfacing can route
    // the decision to that teammate instead of the human.
    let line = build_request(
        &v(&[
            "bus",
            "pub",
            "build",
            "--decision",
            "--to",
            "lead",
            "q=wire order?",
        ]),
        Some(0),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Bus(BusOp::Pub { kind, fields, .. }) => {
            assert_eq!(kind, crate::bus::Kind::DecisionNeeded);
            assert!(fields.contains(&("to".to_string(), "lead".to_string())));
            assert!(fields.iter().any(|(k, _)| k == "q"));
        }
        other => panic!("expected bus pub, got {other:?}"),
    }
    // `--to` without a role is a clear client error.
    assert!(
        build_request(&v(&["bus", "pub", "t", "--decision", "--to"]), None)
            .unwrap_err()
            .contains("--to")
    );
}

#[test]
fn parse_request_carries_the_capability_token() {
    // The server reads the token off the wire to authenticate the caller.
    let req = parse_request(r#"{"caller":2,"token":"deadbeef","cmd":"list"}"#).unwrap();
    assert_eq!(req.caller, Some(2));
    assert_eq!(req.token.as_deref(), Some("deadbeef"));
    // Absent token parses as None (an unauthenticated request).
    let req = parse_request(r#"{"cmd":"list"}"#).unwrap();
    assert_eq!(req.token, None);
}

#[test]
fn build_board_claim_and_release_roundtrip() {
    // Bare claim → default lease (ttl_ms None).
    let claim = build_request(&v(&["board", "claim", "cli"]), Some(0)).unwrap();
    assert!(matches!(
        parse_request(&claim).unwrap().cmd,
        Cmd::Board(BoardOp::Claim { key, ttl_ms: None }) if key == "cli"
    ));
    // `--ttl 90` → 90_000 ms carried through to the server.
    let ttl = build_request(&v(&["board", "claim", "cli", "--ttl", "90"]), Some(0)).unwrap();
    assert!(matches!(
        parse_request(&ttl).unwrap().cmd,
        Cmd::Board(BoardOp::Claim { key, ttl_ms: Some(90_000) }) if key == "cli"
    ));
    // release → BoardOp::Release.
    let rel = build_request(&v(&["board", "release", "cli"]), Some(0)).unwrap();
    assert!(matches!(
        parse_request(&rel).unwrap().cmd,
        Cmd::Board(BoardOp::Release { key }) if key == "cli"
    ));
    // A malformed --ttl is a clear client error, not a silent default.
    assert!(
        build_request(&v(&["board", "claim", "cli", "--ttl", "soon"]), Some(0))
            .unwrap_err()
            .contains("--ttl")
    );
}

#[test]
fn build_spawn_with_identity_roundtrips() {
    let line = build_request(
        &v(&[
            "spawn",
            "--role",
            "dev_1",
            "--identity",
            "work",
            "--",
            "claude",
        ]),
        Some(0),
    )
    .unwrap();
    let req = parse_request(&line).unwrap();
    match req.cmd {
        Cmd::Spawn(sp) => {
            assert_eq!(sp.role.as_deref(), Some("dev_1"));
            assert_eq!(sp.identity.as_deref(), Some("work"));
            assert_eq!(sp.argv, v(&["claude"]));
        }
        _ => panic!("expected spawn"),
    }
}

#[test]
fn spawn_without_identity_leaves_it_none() {
    let line = build_request(&v(&["spawn", "--", "claude"]), None).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.identity, None),
        _ => panic!("expected spawn"),
    }
}

#[test]
fn build_and_parse_kill_request() {
    let req = parse_request(&build_request(&v(&["kill", "dev_1"]), Some(1)).unwrap()).unwrap();
    assert_eq!(req.caller, Some(1));
    assert_eq!(
        req.cmd,
        Cmd::Kill(KillReq {
            target: "dev_1".into()
        })
    );
}

#[test]
fn build_and_parse_audit_with_and_without_tail() {
    let tailed = parse_request(&build_request(&v(&["audit", "20"]), None).unwrap()).unwrap();
    assert_eq!(tailed.cmd, Cmd::Audit(AuditReq { tail: Some(20) }));
    let all = parse_request(&build_request(&v(&["audit"]), None).unwrap()).unwrap();
    assert_eq!(all.cmd, Cmd::Audit(AuditReq { tail: None }));
}
