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
            .to_string()
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
            .to_string()
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
            .to_string()
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

#[test]
fn server_clamps_a_claim_ttl_to_the_lease_cap() {
    // A raw request can carry any ttl_ms; the board adds it to now_ms, so an
    // uncapped one overflows the lease.
    let req = parse_request(
        r#"{"caller":0,"cmd":"board","op":"claim","key":"k","ttl_ms":9223372036854775000}"#,
    )
    .unwrap();
    assert!(matches!(
        req.cmd,
        Cmd::Board(BoardOp::Claim { ttl_ms: Some(ms), .. }) if ms == MAX_TTL_MS
    ));
}

// ---- hello / report / whoami: the mod's verbs ------------------------------

#[test]
fn hello_builds_and_parses_with_caps() {
    let line = build_request(
        &v(&["hello", "mod=0.1.0", "engine=2.1.288", "caps=status,inbox"]),
        Some(3),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Hello(h) => {
            assert_eq!(h.version, "0.1.0");
            assert_eq!(h.engine.as_deref(), Some("2.1.288"));
            assert_eq!(h.caps, v(&["status", "inbox"]));
        }
        other => panic!("expected hello, got {other:?}"),
    }
    assert!(
        build_request(&v(&["hello"]), None).is_err(),
        "mod= is required"
    );
    assert!(build_request(&v(&["hello", "mod=1", "bogus=2"]), None).is_err());
    // A caps string on the wire (a hand-rolled request) is accepted too.
    match parse_request(r#"{"cmd":"hello","mod":"x","caps":"status, guard,"}"#)
        .unwrap()
        .cmd
    {
        Cmd::Hello(h) => assert_eq!(h.caps, v(&["status", "guard"])),
        other => panic!("expected hello, got {other:?}"),
    }
    assert!(parse_request(r#"{"cmd":"hello","mod":"x","caps":7}"#).is_err());
    assert!(parse_request(r#"{"cmd":"hello","mod":"  "}"#).is_err());
}

/// A mod speaks for the pane it runs in and no other: a `target` on either
/// verb is refused before any handler sees it.
#[test]
fn hello_and_report_refuse_a_target() {
    let err = parse_request(r#"{"cmd":"hello","mod":"0.1.0","target":"2"}"#).unwrap_err();
    assert!(err.contains("caller only"), "{err}");
    let err = parse_request(r#"{"cmd":"report","status":"idle","target":"lead"}"#).unwrap_err();
    assert!(err.contains("caller only"), "{err}");
}

#[test]
fn report_builds_typed_numbers_and_parses() {
    let line = build_request(
        &v(&[
            "report",
            "status=waiting-approval",
            "context=42",
            "cost=1.25",
            "turns=7",
            "answer=all",
            "done",
            "here",
        ]),
        Some(1),
    )
    .unwrap();
    assert!(line.contains("\"context\":42"), "typed on the wire: {line}");
    match parse_request(&line).unwrap().cmd {
        Cmd::Report(r) => {
            assert_eq!(r.status, Some(crate::modstate::ModStatus::WaitingApproval));
            assert_eq!(r.context_pct, Some(42));
            assert_eq!(r.cost_usd, Some(1.25));
            assert_eq!(r.turns, Some(7));
            assert_eq!(r.answer.as_deref(), Some("all done here"));
            assert_eq!(r.reason, None);
        }
        other => panic!("expected report, got {other:?}"),
    }
}

#[test]
fn report_refuses_nonsense_on_both_sides() {
    assert!(build_request(&v(&["report"]), None).is_err());
    assert!(build_request(&v(&["report", "context=101"]), None).is_err());
    assert!(build_request(&v(&["report", "cost=-1"]), None).is_err());
    assert!(build_request(&v(&["report", "turns=x"]), None).is_err());
    assert!(build_request(&v(&["report", "mood=great"]), None).is_err());
    let err = parse_request(r#"{"cmd":"report","status":"busy"}"#).unwrap_err();
    assert!(err.contains("busy"), "names the bad label: {err}");
    assert!(
        parse_request(r#"{"cmd":"report"}"#).is_err(),
        "an empty report"
    );
    assert!(parse_request(r#"{"cmd":"report","context":250}"#).is_err());
    assert!(parse_request(r#"{"cmd":"report","cost":-0.5}"#).is_err());
    assert!(parse_request(r#"{"cmd":"report","turns":-1}"#).is_err());
}

#[test]
fn whoami_builds_and_parses_and_the_mod_verbs_need_a_token() {
    let line = build_request(&v(&["whoami"]), None).unwrap();
    assert!(matches!(parse_request(&line).unwrap().cmd, Cmd::Whoami));
    let verbs = [
        Cmd::Whoami,
        Cmd::Hello(HelloReq {
            version: "1".to_string(),
            engine: None,
            caps: Vec::new(),
        }),
        Cmd::Report(ReportReq {
            turns: Some(1),
            ..ReportReq::default()
        }),
    ];
    for c in verbs {
        assert!(
            !c.is_read_only(),
            "{c:?} is about the caller: needs a token"
        );
    }
}

/// The audit log is readable by anyone in the subtree; a report's answer is
/// the model's output and never goes in it.
#[test]
fn report_audit_label_never_carries_the_answer() {
    let c = Cmd::Report(ReportReq {
        answer: Some("secret text".to_string()),
        status: Some(crate::modstate::ModStatus::Idle),
        ..ReportReq::default()
    });
    let (action, detail) = c.audit_label();
    assert_eq!(action, "report");
    assert!(!detail.contains("secret"), "{detail}");
    assert!(detail.contains("answer_len=11"), "{detail}");
    assert!(detail.contains("status=idle"), "{detail}");
}

// ---- answer / wait: collecting a subagent pane's answer ----------------------

#[test]
fn answer_builds_parses_and_needs_a_token() {
    let line = build_request(&v(&["answer", "dev_1"]), Some(0)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Answer(a) => assert_eq!(a.target, "dev_1"),
        other => panic!("expected answer, got {other:?}"),
    }
    assert!(build_request(&v(&["answer"]), None).is_err());
    assert!(!Cmd::Answer(AnswerReq {
        target: "x".to_string()
    })
    .is_read_only());
    let (action, detail) = Cmd::Answer(AnswerReq {
        target: "dev_1".to_string(),
    })
    .audit_label();
    assert_eq!((action, detail.as_str()), ("answer", "target=dev_1"));
}

#[test]
fn wait_parses_its_flags_and_refuses_nonsense() {
    let spec = parse_wait(&v(&["wait", "dev_1"])).unwrap();
    assert_eq!(
        spec,
        WaitSpec {
            target: "dev_1".to_string(),
            what: WaitFor::Answer,
            timeout_s: 300,
            after: 0
        }
    );
    let spec = parse_wait(&v(&[
        "wait",
        "3",
        "--for",
        "idle",
        "--timeout",
        "7",
        "--after",
        "4",
    ]))
    .unwrap();
    assert_eq!(
        (spec.what, spec.timeout_s, spec.after),
        (WaitFor::Idle, 7, 4)
    );
    assert_eq!(
        parse_wait(&v(&["wait", "3", "--for", "exit"]))
            .unwrap()
            .what,
        WaitFor::Exit
    );
    assert!(parse_wait(&v(&["wait"])).is_err());
    assert!(parse_wait(&v(&["wait", "3", "--for", "done"])).is_err());
    assert!(parse_wait(&v(&["wait", "3", "--timeout", "0"])).is_err());
    assert!(parse_wait(&v(&["wait", "3", "--timeout", "999999"])).is_err());
    assert!(parse_wait(&v(&["wait", "3", "--bogus"])).is_err());
}

#[test]
fn wait_ends_on_a_newer_answer_an_idle_status_or_a_gone_pane() {
    let answer = WaitSpec {
        target: "3".to_string(),
        what: WaitFor::Answer,
        timeout_s: 1,
        after: 2,
    };
    assert!(!wait_done(
        &answer,
        r#"{"ok":true,"pane":3,"status":"working","seq":null,"answer":null}"#
    ));
    assert!(!wait_done(
        &answer,
        r#"{"ok":true,"pane":3,"status":"waiting-prompt","seq":2,"answer":"old"}"#
    ));
    assert!(wait_done(
        &answer,
        r#"{"ok":true,"pane":3,"status":"waiting-prompt","seq":3,"answer":"new"}"#
    ));
    assert!(!wait_done(&answer, r#"{"ok":false,"err":"no such pane"}"#));
    assert!(!wait_done(&answer, "not json"));
    let idle = WaitSpec {
        what: WaitFor::Idle,
        ..answer.clone()
    };
    assert!(!wait_done(
        &idle,
        r#"{"ok":true,"pane":3,"status":"working","idle_ms":1}"#
    ));
    assert!(wait_done(
        &idle,
        r#"{"ok":true,"pane":3,"status":"waiting-prompt","idle_ms":1}"#
    ));
    assert!(wait_done(
        &idle,
        r#"{"ok":true,"pane":3,"status":"errored","idle_ms":1}"#
    ));
    assert!(!wait_done(
        &idle,
        r#"{"ok":true,"pane":3,"status":null,"idle_ms":1}"#
    ));
    let exit = WaitSpec {
        what: WaitFor::Exit,
        ..answer
    };
    assert!(!wait_done(
        &exit,
        r#"{"ok":true,"pane":3,"status":"working","idle_ms":1}"#
    ));
    assert!(wait_done(&exit, r#"{"ok":false,"err":"no such pane 3"}"#));
}

#[test]
fn inbox_builds_parses_needs_a_token_and_refuses_a_target() {
    let line = build_request(&v(&["inbox"]), Some(2)).unwrap();
    assert!(
        !line.contains("wait"),
        "a plain inbox carries no wait field: {line}"
    );
    assert!(matches!(
        parse_request(&line).unwrap().cmd,
        Cmd::Inbox(InboxReq { wait: false })
    ));
    assert!(!Cmd::Inbox(InboxReq::default()).is_read_only());
    let err = parse_request(r#"{"cmd":"inbox","target":"3"}"#).unwrap_err();
    assert!(err.contains("caller only"), "{err}");
    let item = |kind: &str, text: &str, id: Option<u64>| InboxItem {
        kind: kind.to_string(),
        text: text.to_string(),
        id,
    };
    let reply = reply_inbox(vec![
        item("send", "do x", None),
        item("wake", "[atrium bus #1 fyi] y", None),
        item("ask", "what now?", Some(3)),
    ]);
    assert_eq!(
        reply.to_json(),
        r#"{"ok":true,"items":[{"kind":"send","text":"do x"},{"kind":"wake","text":"[atrium bus #1 fyi] y"},{"kind":"ask","text":"what now?","id":3}]}"#
    );
}

/// `inbox --wait` asks the server to hold the request until a delivery lands.
/// The flag rides as `"wait":true`; an older server that does not know the
/// field answers at once, which the mod detects and falls back on. Any other
/// argument is a usage error rather than a silent poll.
#[test]
fn inbox_wait_builds_parses_and_is_labelled_for_the_audit() {
    let line = build_request(&v(&["inbox", "--wait"]), Some(2)).unwrap();
    assert!(line.contains(r#""wait":true"#), "{line}");
    let cmd = parse_request(&line).unwrap().cmd;
    assert_eq!(cmd, Cmd::Inbox(InboxReq { wait: true }));
    assert!(!cmd.is_read_only(), "a held inbox still takes deliveries");
    assert_eq!(cmd.audit_label(), ("inbox", "wait".to_string()));
    assert_eq!(
        Cmd::Inbox(InboxReq { wait: false }).audit_label(),
        ("inbox", String::new())
    );
    // Not a boolean: a poll, never an error — the wire stays lenient.
    let cmd = parse_request(r#"{"cmd":"inbox","wait":"yes"}"#)
        .unwrap()
        .cmd;
    assert_eq!(cmd, Cmd::Inbox(InboxReq { wait: false }));
    let err = build_request(&v(&["inbox", "--now"]), Some(2)).unwrap_err();
    assert!(err.to_string().contains("--now"), "{err}");
    let err = build_request(&v(&["inbox", "--wait", "3"]), Some(2)).unwrap_err();
    assert!(err.to_string().contains("--wait"), "{err}");
}

#[test]
fn report_carries_captions_touches_and_ask_replies() {
    let line = build_request(
        &v(&[
            "report",
            "doing=running the tests",
            "touched=/w/a/src/x.rs",
            "touched=/w/a/My Files/y,z.rs",
            "touched=",
        ]),
        Some(2),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Report(r) => {
            assert_eq!(r.doing.as_deref(), Some("running the tests"));
            assert_eq!(
                r.touched,
                vec![
                    "/w/a/src/x.rs".to_string(),
                    "/w/a/My Files/y,z.rs".to_string()
                ],
                "one path per token: spaces and commas survive; an empty one is dropped"
            );
            assert_eq!(r.ask, None);
        }
        other => panic!("expected report, got {other:?}"),
    }
    let line = build_request(&v(&["report", "ask=7", "reply=fine, thanks"]), Some(2)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Report(r) => assert_eq!(r.ask, Some((7, "fine, thanks".to_string()))),
        other => panic!("expected report, got {other:?}"),
    }
    // A caption alone is a report; an empty one clears it.
    let line = build_request(&v(&["report", "doing="]), Some(2)).unwrap();
    assert!(
        matches!(parse_request(&line).unwrap().cmd, Cmd::Report(r) if r.doing.as_deref() == Some(""))
    );
    // A reply must name its ask, and the id must be one.
    assert!(build_request(&v(&["report", "ask=x", "reply=y"]), Some(2)).is_err());
    assert!(build_request(&v(&["report", "ask=0", "reply=y"]), Some(2)).is_err());
    let err = parse_request(r#"{"cmd":"report","reply":"y"}"#).unwrap_err();
    assert!(err.contains("names its ask"), "{err}");
    let err = parse_request(r#"{"cmd":"report","touched":[1]}"#).unwrap_err();
    assert!(err.contains("touched"), "{err}");
    let line = parse_request(r#"{"cmd":"report","touched":"/one"}"#).unwrap();
    assert!(matches!(line.cmd, Cmd::Report(r) if r.touched == vec!["/one".to_string()]));
}

#[test]
fn ask_asked_and_who_build_parse_and_label() {
    let line = build_request(
        &v(&["ask", "dev_1", "what", "are", "you", "doing?"]),
        Some(0),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Ask(a) => {
            assert_eq!(a.target, "dev_1");
            assert_eq!(a.text, "what are you doing?");
            assert!(!Cmd::Ask(a.clone()).is_read_only());
            let (action, detail) = Cmd::Ask(a).audit_label();
            assert_eq!((action, detail.as_str()), ("ask", "target=dev_1 len=19"));
        }
        other => panic!("expected ask, got {other:?}"),
    }
    assert!(
        build_request(&v(&["ask", "dev_1"]), Some(0)).is_err(),
        "no question"
    );
    assert!(build_request(&v(&["ask", "dev_1", "  "]), Some(0)).is_err());
    let line = build_request(&v(&["asked", "3", "12"]), Some(0)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Asked(a) => {
            assert_eq!((a.target.as_str(), a.id), ("3", 12));
            assert!(!Cmd::Asked(a.clone()).is_read_only());
            assert_eq!(Cmd::Asked(a).audit_label().1, "target=3 id=12");
        }
        other => panic!("expected asked, got {other:?}"),
    }
    assert!(build_request(&v(&["asked", "3", "0"]), Some(0)).is_err());
    assert!(build_request(&v(&["asked", "3"]), Some(0)).is_err());
    let line = build_request(&v(&["who", "src/x.rs"]), None).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Who(w) => {
            assert_eq!(w.path, "src/x.rs");
            assert!(
                !Cmd::Who(w.clone()).is_read_only(),
                "a token-matched caller's own subtree, like status"
            );
            assert_eq!(Cmd::Who(w).audit_label().1, "path=src/x.rs");
        }
        other => panic!("expected who, got {other:?}"),
    }
    assert!(build_request(&v(&["who"]), None).is_err());
    assert!(parse_request(r#"{"cmd":"who","path":" "}"#).is_err());
}

#[test]
fn ask_parses_its_flags_and_ends_on_a_reply_or_a_refusal() {
    let spec = parse_ask(&v(&["ask", "dev_1", "--timeout", "30", "what", "now"])).unwrap();
    assert_eq!(
        spec,
        AskSpec {
            target: "dev_1".to_string(),
            text: "what now".to_string(),
            timeout_s: 30
        }
    );
    let spec = parse_ask(&v(&["ask", "3", "is --timeout a flag here?"])).unwrap();
    assert_eq!(
        spec.text, "is --timeout a flag here?",
        "a flag after the question starts is text"
    );
    assert_eq!(spec.timeout_s, ASK_DEFAULT_TIMEOUT_S);
    assert!(parse_ask(&v(&["ask", "3", "--timeout", "0", "q"])).is_err());
    assert!(parse_ask(&v(&["ask", "3", "--timeout", "9999", "q"])).is_err());
    assert!(parse_ask(&v(&["ask", "3"])).is_err());
    assert!(parse_ask(&v(&["ask"])).is_err());
    assert!(
        !ask_done(r#"{"ok":true,"pane":3,"id":1,"reply":null}"#),
        "pending"
    );
    assert!(ask_done(r#"{"ok":true,"pane":3,"id":1,"reply":"fine"}"#));
    assert!(
        ask_done(r#"{"ok":false,"err":"no such ask"}"#),
        "a refusal ends it"
    );
    assert!(!ask_done("not json"));
    let reply = reply_asked(AgentId(3), 1, None).to_json();
    assert_eq!(reply, r#"{"ok":true,"pane":3,"id":1,"reply":null}"#);
    let who = reply_who(
        "src/x.rs",
        vec![WhoEntry {
            pane: AgentId(2),
            role: Some("dev_1".to_string()),
            key: "src/x.rs".to_string(),
            path: "/w/a/src/x.rs".to_string(),
            first_ms: 5,
            last_ms: 9,
            live: true,
        }],
    )
    .to_json();
    assert_eq!(
        who,
        r#"{"ok":true,"path":"src/x.rs","panes":[{"pane":2,"role":"dev_1","key":"src/x.rs","path":"/w/a/src/x.rs","first_ms":5,"last_ms":9,"live":true}]}"#
    );
}

// ---- Wire-golden rows for the host-read surface (docs/control-plane.md,
// "Reading the control plane from a host"). A host terminal reads these
// token-free replies; their shapes are additive-only from 0.43.0 on, so a
// failing row here is a wire change and belongs in the changelog as one.

#[test]
fn golden_list_reply_names_the_atrium_and_each_pane() {
    let reply = Reply::Tree(vec![
        ListNode {
            id: AgentId(1),
            parent: None,
            role: None,
            title: "cmd".to_string(),
            depth: 0,
            status: None,
            idle_ms: 0,
        },
        ListNode {
            id: AgentId(3),
            parent: Some(AgentId(1)),
            role: Some("lead".to_string()),
            title: "claude".to_string(),
            depth: 1,
            status: Some("working".to_string()),
            idle_ms: 1200,
        },
    ]);
    assert_eq!(
        reply.to_json(),
        format!(
            r#"{{"ok":true,"atrium":"{}","tree":[{{"id":1,"parent":null,"role":null,"title":"cmd","depth":0,"status":null,"idle_ms":0}},{{"id":3,"parent":1,"role":"lead","title":"claude","depth":1,"status":"working","idle_ms":1200}}]}}"#,
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn golden_status_reply_for_one_pane() {
    let reply = Reply::StatusOne {
        pane: AgentId(3),
        status: Some("waiting-approval".to_string()),
        idle_ms: 7000,
        doing: None,
    };
    assert_eq!(
        reply.to_json(),
        r#"{"ok":true,"pane":3,"status":"waiting-approval","idle_ms":7000}"#
    );
    let unbound = Reply::StatusOne {
        pane: AgentId(1),
        status: None,
        idle_ms: 0,
        doing: None,
    };
    assert_eq!(
        unbound.to_json(),
        r#"{"ok":true,"pane":1,"status":null,"idle_ms":0}"#
    );
}

#[test]
fn golden_board_list_bus_feed_and_topics_replies() {
    let board = json::parse(r#"{"m1":{"status":"DONE","owner":"dev_1"}}"#).unwrap();
    assert_eq!(
        Reply::BoardList(board).to_json(),
        r#"{"ok":true,"board":{"m1":{"status":"DONE","owner":"dev_1"}}}"#
    );
    let feed = json::parse(r#"[{"seq":1,"topic":"work","kind":"fyi","msg":"hi"}]"#).unwrap();
    assert_eq!(
        Reply::BusFeed { feed, cursor: 1 }.to_json(),
        r#"{"ok":true,"feed":[{"seq":1,"topic":"work","kind":"fyi","msg":"hi"}],"cursor":1}"#
    );
    assert_eq!(
        Reply::BusTopics(vec![("work".to_string(), 2)]).to_json(),
        r#"{"ok":true,"topics":[{"topic":"work","subs":2}]}"#
    );
}

#[test]
fn golden_error_reply_is_the_only_failure_shape() {
    assert_eq!(
        reply_err("unauthenticated: no token").to_json(),
        r#"{"ok":false,"err":"unauthenticated: no token"}"#
    );
}

#[test]
fn golden_ping_reply_names_the_atrium_and_its_uptime() {
    assert_eq!(
        reply_pong(90_500).to_json(),
        format!(
            r#"{{"ok":true,"atrium":"{}","uptime_ms":90500}}"#,
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn golden_metrics_reply_carries_every_state_and_counter() {
    let mut m = Metrics {
        uptime_ms: 90_500,
        wakes_delivered: 12,
        held_inboxes: 2,
        bus_depth: 40,
        bus_last_seq: 57,
        open_decisions: 1,
        ..Metrics::default()
    };
    for status in [
        Some("working"),
        Some("working"),
        Some("waiting-approval"),
        Some("idle"),
        Some("errored"),
        None,
        // Not a status word: counted as unbound, never a key of its own.
        Some("a\"b"),
    ] {
        m.count_pane(status);
    }
    assert_eq!(
        reply_metrics(m).to_json(),
        format!(
            r#"{{"ok":true,"atrium":"{}","uptime_ms":90500,"panes":{{"working":2,"waiting-approval":1,"waiting-prompt":0,"idle":1,"errored":1,"ended":0,"unbound":2}},"wakes_delivered":12,"held_inboxes":2,"bus_depth":40,"bus_last_seq":57,"open_decisions":1}}"#,
            env!("CARGO_PKG_VERSION")
        )
    );
}

/// `ping` and `metrics` round-trip argv → request → `Cmd`, need no token, and
/// `metrics` takes no argument.
#[test]
fn ping_and_metrics_build_parse_and_are_read_only() {
    for (argv, want) in [(&["ping"][..], Cmd::Ping), (&["metrics"][..], Cmd::Metrics)] {
        let req = build_request(&v(argv), None).expect("build");
        let cmd = parse_request(&req).expect("parse").cmd;
        assert_eq!(cmd, want);
        assert!(cmd.is_read_only(), "{argv:?} is a token-free read");
        assert_eq!(cmd.audit_label(), (argv[0], String::new()));
    }
    assert!(build_request(&v(&["metrics", "extra"]), None).is_err());
}

/// One parsed line of Prometheus text exposition.
#[derive(Debug, PartialEq)]
enum Expo {
    Help(String),
    Type(String, String),
    Sample {
        name: String,
        labels: Vec<(String, String)>,
        value: f64,
    },
}

fn metric_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Parse one line by the exposition grammar, or say what is wrong with it:
/// `# HELP <name> <text>`, `# TYPE <name> <counter|gauge>`, or
/// `<name>[{<label>="<value>",…}] <number>` with only `\\`, `\"` and `\n`
/// escapes inside a label value.
fn expo_line(line: &str) -> Result<Expo, String> {
    if let Some(rest) = line.strip_prefix("# HELP ") {
        let (name, text) = rest.split_once(' ').ok_or("HELP without text")?;
        if !metric_name(name) || text.is_empty() {
            return Err(format!("bad HELP: {line:?}"));
        }
        return Ok(Expo::Help(name.to_string()));
    }
    if let Some(rest) = line.strip_prefix("# TYPE ") {
        let (name, kind) = rest.split_once(' ').ok_or("TYPE without a type")?;
        if !metric_name(name) || !matches!(kind, "counter" | "gauge") {
            return Err(format!("bad TYPE: {line:?}"));
        }
        return Ok(Expo::Type(name.to_string(), kind.to_string()));
    }
    let name_end = line
        .find(['{', ' '])
        .ok_or_else(|| format!("no value: {line:?}"))?;
    let name = &line[..name_end];
    if !metric_name(name) {
        return Err(format!("bad name: {line:?}"));
    }
    let mut labels = Vec::new();
    let mut rest = &line[name_end..];
    if let Some(mut body) = rest.strip_prefix('{') {
        loop {
            let eq = body.find("=\"").ok_or("label without =\"")?;
            let label = &body[..eq];
            if !metric_name(label) || label.contains(':') {
                return Err(format!("bad label name: {line:?}"));
            }
            let mut value = String::new();
            let mut chars = body[eq + 2..].char_indices();
            let close = loop {
                match chars.next().ok_or("unterminated label value")? {
                    (at, '"') => break eq + 2 + at,
                    (_, '\\') => match chars.next().ok_or("dangling backslash")?.1 {
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        'n' => value.push('\n'),
                        other => return Err(format!("bad escape \\{other}: {line:?}")),
                    },
                    (_, c) => value.push(c),
                }
            };
            labels.push((label.to_string(), value));
            body = &body[close + 1..];
            if let Some(more) = body.strip_prefix(',') {
                body = more;
            } else {
                rest = body.strip_prefix('}').ok_or("labels not closed")?;
                break;
            }
        }
    }
    let value = rest
        .strip_prefix(' ')
        .ok_or_else(|| format!("no space before the value: {line:?}"))?;
    let value = value
        .parse::<f64>()
        .map_err(|_| format!("bad value: {line:?}"))?;
    Ok(Expo::Sample {
        name: name.to_string(),
        labels,
        value,
    })
}

/// A sample: its name, its labels in order, its value.
type Sample = (String, Vec<(String, String)>, f64);

/// Parse a whole exposition: it ends in a newline, has no blank line, every
/// name is prefixed `atrium_`, and every sample follows its family's `HELP`
/// and `TYPE`. Returns the samples.
fn expo(text: &str) -> Vec<Sample> {
    assert!(text.ends_with('\n'), "ends in a newline: {text:?}");
    let (mut helped, mut typed, mut samples) = (Vec::new(), Vec::new(), Vec::new());
    for line in text.strip_suffix('\n').unwrap().split('\n') {
        match expo_line(line).unwrap_or_else(|e| panic!("{e}\nin:\n{text}")) {
            Expo::Help(name) => helped.push(name),
            Expo::Type(name, _) => typed.push(name),
            Expo::Sample {
                name,
                labels,
                value,
            } => {
                assert!(name.starts_with("atrium_"), "{name}");
                assert!(helped.contains(&name), "{name} has no HELP before it");
                assert!(typed.contains(&name), "{name} has no TYPE before it");
                samples.push((name, labels, value));
            }
        }
    }
    samples
}

#[test]
fn metrics_text_is_valid_prometheus_exposition() {
    let mut m = Metrics {
        uptime_ms: 90_500,
        wakes_delivered: 12,
        held_inboxes: 2,
        bus_depth: 40,
        bus_last_seq: 57,
        open_decisions: 1,
        ..Metrics::default()
    };
    m.count_pane(Some("working"));
    m.count_pane(Some("waiting-approval"));
    m.count_pane(None);
    let text = render::render_metrics(&reply_metrics(m).to_json()).expect("renders");
    let samples = expo(&text);
    let value = |name: &str, labels: &[(&str, &str)]| {
        let labels: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        samples
            .iter()
            .find(|(n, l, _)| n == name && *l == labels)
            .unwrap_or_else(|| panic!("no {name}{labels:?} in:\n{text}"))
            .2
    };
    assert_eq!(
        value(
            "atrium_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))]
        ),
        1.0
    );
    assert_eq!(value("atrium_uptime_seconds", &[]), 90.5);
    assert_eq!(value("atrium_panes", &[("state", "working")]), 1.0);
    assert_eq!(value("atrium_panes", &[("state", "waiting-approval")]), 1.0);
    assert_eq!(value("atrium_panes", &[("state", "idle")]), 0.0);
    assert_eq!(value("atrium_panes", &[("state", "unbound")]), 1.0);
    assert_eq!(value("atrium_wakes_delivered_total", &[]), 12.0);
    assert_eq!(value("atrium_held_inboxes", &[]), 2.0);
    assert_eq!(value("atrium_bus_depth", &[]), 40.0);
    assert_eq!(value("atrium_bus_last_seq", &[]), 57.0);
    assert_eq!(value("atrium_open_decisions", &[]), 1.0);
    // One series per fixed state, and no label but `version` and `state`.
    let states: Vec<&str> = samples
        .iter()
        .filter(|(n, _, _)| n == "atrium_panes")
        .map(|(_, l, _)| l[0].1.as_str())
        .collect();
    assert_eq!(states, PANE_STATES);
    assert!(samples
        .iter()
        .flat_map(|(_, l, _)| l)
        .all(|(k, _)| k == "version" || k == "state"));
}

/// Whatever answers at the endpoint, its reply cannot put a line of its own,
/// or a pane's text, into the scrape: the version is escaped as a label value
/// and a pane state the client does not know is not printed.
#[test]
fn metrics_text_escapes_the_version_and_prints_no_unknown_state() {
    use json::Value;
    use reply::obj;
    let hostile = "1\"} 1\natrium_evil 9\n# \\";
    let reply = obj(vec![
        ("ok", Value::Bool(true)),
        ("atrium", Value::String(hostile.to_string())),
        ("uptime_ms", Value::Number(json::Number::Int(1))),
        (
            "panes",
            obj(vec![
                ("working", Value::Number(json::Number::Int(3))),
                (
                    "lead\"} 1\natrium_evil 9",
                    Value::Number(json::Number::Int(7)),
                ),
            ]),
        ),
    ])
    .to_string();
    let text = render::render_metrics(&reply).expect("renders");
    let samples = expo(&text);
    assert!(samples.iter().all(|(n, _, _)| n != "atrium_evil"), "{text}");
    let version = &samples
        .iter()
        .find(|(n, _, _)| n == "atrium_build_info")
        .expect("build info")
        .1;
    assert_eq!(version[0], ("version".to_string(), hostile.to_string()));
    assert_eq!(
        samples
            .iter()
            .filter(|(n, _, _)| n == "atrium_panes")
            .count(),
        PANE_STATES.len()
    );
    assert_eq!(render::escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    // A refusal, or a reply of another verb, is not rendered at all.
    assert!(render::render_metrics(r#"{"ok":false,"err":"no"}"#).is_none());
    assert!(render::render_metrics(&reply_pong(5).to_json()).is_none());
}

#[test]
fn spawn_no_focus_parses_and_defaults_to_focus() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let line = build_request(
        &args(&["spawn", "--here", "--no-focus", "--", "claude"]),
        Some(3),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => {
            assert!(!sp.new_window);
            assert!(!sp.focus);
        }
        other => panic!("{other:?}"),
    }
    let line = build_request(&args(&["spawn", "--here", "--", "claude"]), Some(3)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => assert!(sp.focus, "focus is the default"),
        other => panic!("{other:?}"),
    }
}

/// `spawn --here --beside 4` names the pane to tile next to (the tmux shim's
/// `split-window -t %4`); it is a number, it needs `--here`, and without it the
/// request carries no anchor so the server falls back to the caller.
#[test]
fn spawn_beside_parses_a_pane_id_and_needs_here() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let line = build_request(
        &args(&[
            "spawn",
            "--here",
            "--beside",
            "4",
            "--no-focus",
            "--",
            "claude",
        ]),
        Some(3),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => {
            assert!(!sp.new_window);
            assert!(!sp.focus);
            assert_eq!(sp.beside, Some(4));
        }
        other => panic!("{other:?}"),
    }
    let line = build_request(&args(&["spawn", "--here", "--", "claude"]), Some(3)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.beside, None, "no --beside: beside the caller"),
        other => panic!("{other:?}"),
    }
    let err = build_request(
        &args(&["spawn", "--here", "--beside", "dev_1", "--", "claude"]),
        Some(3),
    )
    .unwrap_err();
    assert!(err.message().contains("not a pane id"), "{err:?}");
    let err =
        build_request(&args(&["spawn", "--beside", "4", "--", "claude"]), Some(3)).unwrap_err();
    assert!(err.message().contains("--here"), "{err:?}");
    let err = build_request(&args(&["spawn", "--here", "--beside"]), Some(3)).unwrap_err();
    assert!(err.message().contains("needs a pane id"), "{err:?}");
}

/// `spawn --here --size 70%` (the tmux shim's `-l 70%`) asks the new pane's
/// share of the anchor's span; a bare number is cells. It is validated in the
/// client, carried as text, parsed again by the server, and needs `--here`.
#[test]
fn spawn_size_parses_a_percent_or_cells_and_rejects_the_rest() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let line = build_request(
        &args(&["spawn", "--here", "--size", "70%", "--", "claude"]),
        Some(3),
    )
    .unwrap();
    assert!(line.contains("\"size\":\"70%\""), "{line}");
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => {
            assert_eq!(sp.size, Some(SpawnSize::Percent(70)));
            assert_eq!(sp.size.unwrap().permille(120), 700);
            assert_eq!(sp.size.unwrap().as_flag(), "70%");
        }
        other => panic!("{other:?}"),
    }
    let line = build_request(
        &args(&[
            "spawn", "--here", "--beside", "4", "--size", "20", "--", "claude",
        ]),
        Some(3),
    )
    .unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => {
            assert_eq!(sp.size, Some(SpawnSize::Cells(20)));
            assert_eq!(sp.beside, Some(4));
            // 20 of 80 cells is a quarter; past the span it is the whole span.
            assert_eq!(sp.size.unwrap().permille(80), 250);
            assert_eq!(sp.size.unwrap().permille(10), 1000);
            assert_eq!(sp.size.unwrap().as_flag(), "20");
        }
        other => panic!("{other:?}"),
    }
    let line = build_request(&args(&["spawn", "--here", "--", "claude"]), Some(3)).unwrap();
    match parse_request(&line).unwrap().cmd {
        Cmd::Spawn(sp) => assert_eq!(sp.size, None, "no --size: an equal grid"),
        other => panic!("{other:?}"),
    }
    for bad in ["0%", "100%", "abc", "0", "%", "7%%"] {
        let err = build_request(
            &args(&["spawn", "--here", "--size", bad, "--", "claude"]),
            Some(3),
        )
        .unwrap_err();
        assert!(err.message().contains("not a size"), "{bad}: {err:?}");
    }
    // A negative reads as a flag with no value: refused too, by `value_at`.
    build_request(
        &args(&["spawn", "--here", "--size", "-5", "--", "claude"]),
        Some(3),
    )
    .unwrap_err();
    let err =
        build_request(&args(&["spawn", "--size", "70%", "--", "claude"]), Some(3)).unwrap_err();
    assert!(err.message().contains("--here"), "{err:?}");
    let err = build_request(&args(&["spawn", "--here", "--size"]), Some(3)).unwrap_err();
    assert!(err.message().contains("needs a value"), "{err:?}");
    // A hand-rolled request with a bad size is refused, not silently equal.
    let err = parse_request(r#"{"cmd":"spawn","argv":["claude"],"window":false,"size":"150%"}"#)
        .unwrap_err();
    assert!(err.contains("not a size"), "{err}");
}

/// `ctl command <target> /<name>`: one slash command and nothing after it.
/// The client refuses anything else as a usage error; the server refuses a
/// request that carries more than the name.
#[test]
fn build_command_takes_one_slash_command_and_no_arguments() {
    let line = build_request(&v(&["command", "dev_1", "/compact"]), Some(0)).unwrap();
    assert_eq!(
        parse_request(&line).unwrap().cmd,
        Cmd::Command(CommandReq {
            target: "dev_1".to_string(),
            name: "/compact".to_string()
        })
    );
    for args in [
        vec!["command"],
        vec!["command", "dev_1"],
        vec!["command", "dev_1", "/compact", "now"],
        vec!["command", "dev_1", "compact"],
        vec!["command", "dev_1", "!rm"],
        vec!["command", "dev_1", "/a b"],
    ] {
        assert!(build_request(&v(&args), Some(0)).is_err(), "{args:?}");
    }
    for extra in ["\"args\":[\"x\"]", "\"text\":\"x\"", "\"argv\":[\"x\"]"] {
        let line =
            format!("{{\"cmd\":\"command\",\"target\":\"1\",\"name\":\"/compact\",{extra}}}");
        assert!(parse_request(&line).is_err(), "{line}");
    }
    assert!(!parse_request(&line).unwrap().cmd.is_read_only());
}

/// The audit detail of `who` keeps the end of a long path: the file name is
/// what the reader needs.
#[test]
fn the_audit_label_of_who_keeps_the_file_name_of_a_long_path() {
    let path = format!("/w/{}src/main.rs", "deep/".repeat(100));
    let (action, detail) = Cmd::Who(WhoReq { path }).audit_label();
    assert_eq!(action, "who");
    assert!(detail.ends_with("src/main.rs"), "{detail}");
    assert!(detail.chars().count() <= "path=".len() + crate::audit::FIELD_MAX + 1);
}

// ---- the tmux shim's two requests (Claude Code 2.1.325) ----------------------

/// `spawn --here --hold`: a placeholder. It round-trips with its placement
/// and names nothing a worker would.
#[test]
fn a_hold_spawn_round_trips_with_its_placement_and_no_command() {
    let args = crate::tmux_shim::hold_args(false, Some(4), Some("70%"));
    assert_eq!(
        args,
        v(&[
            "spawn",
            "--here",
            "--hold",
            "--no-focus",
            "--beside",
            "4",
            "--size",
            "70%"
        ])
    );
    let line = build_request(&args, Some(3)).unwrap();
    let Cmd::Spawn(sp) = parse_request(&line).unwrap().cmd else {
        panic!("a spawn");
    };
    assert!(sp.hold);
    assert!(sp.argv.is_empty() && sp.role.is_none() && sp.identity.is_none());
    assert!(sp.mode.is_none() && sp.worktree.is_none());
    assert!(!sp.new_window && !sp.focus);
    assert_eq!(sp.beside, Some(4));
    assert_eq!(sp.size, Some(SpawnSize::Percent(70)));
    assert_eq!(
        Cmd::Spawn(sp).audit_label(),
        ("spawn", "hold beside=4 size=70%".to_string())
    );
    // A `-l` the size grammar refuses is dropped, as for an agent split.
    assert_eq!(
        crate::tmux_shim::hold_args(true, None, Some("100%")),
        v(&["spawn", "--here", "--hold"])
    );
    // Every other spawn is not one.
    let line = build_request(&v(&["spawn", "--here", "--", "claude"]), Some(3)).unwrap();
    let Cmd::Spawn(sp) = parse_request(&line).unwrap().cmd else {
        panic!("a spawn");
    };
    assert!(!sp.hold);
}

/// A hold request that carries anything a worker would is refused by the
/// client and by the server's parser: it cannot be bent into another spawn.
#[test]
fn a_hold_spawn_takes_nothing_that_would_make_it_another_spawn() {
    for args in [
        v(&["spawn", "--hold"]),
        v(&["spawn", "--window", "--hold"]),
        v(&["spawn", "--here", "--hold", "--", "cat"]),
        v(&["spawn", "--here", "--hold", "--"]),
        v(&["spawn", "--here", "--hold", "--role", "dev"]),
        v(&["spawn", "--here", "--hold", "--identity", "work"]),
        v(&["spawn", "--here", "--hold", "--mode", "skip"]),
        v(&["spawn", "--here", "--hold", "--worktree", "w"]),
    ] {
        assert!(build_request(&args, Some(3)).is_err(), "{args:?}");
    }
    for body in [
        r#""hold":true,"argv":["cat"],"window":false"#,
        r#""hold":true,"argv":["claude"],"window":false"#,
        r#""hold":true,"argv":[],"window":true"#,
        r#""hold":true,"argv":[]"#,
        r#""hold":true,"window":false,"role":"dev""#,
        r#""hold":true,"window":false,"identity":"work""#,
        r#""hold":true,"window":false,"mode":"skip""#,
        r#""hold":true,"window":false,"worktree":"w""#,
        r#""hold":"yes","argv":["claude"]"#,
        r#""hold":1,"argv":["claude"]"#,
        r#""hold":null,"argv":["claude"]"#,
        r#""hold":true,"argv":"cat","window":false"#,
    ] {
        let line = format!(r#"{{"cmd":"spawn",{body}}}"#);
        assert!(parse_request(&line).is_err(), "{line}");
    }
    // The least a hand-rolled one may be.
    let Cmd::Spawn(sp) = parse_request(r#"{"cmd":"spawn","hold":true,"window":false}"#)
        .unwrap()
        .cmd
    else {
        panic!("a spawn");
    };
    assert!(sp.hold && sp.argv.is_empty());
    // `"hold": false` is an ordinary spawn, with every rule of one.
    assert!(parse_request(r#"{"cmd":"spawn","hold":false}"#).is_err());
}

/// `respawn <pane> --teammate <line>`: the line travels untouched, and the
/// audit label holds flag names and counts, never a value.
#[test]
fn a_teammate_respawn_carries_its_line_untouched_and_is_audited_without_values() {
    let cmdline = "cd '/w/my dir' && env CLAUDECODE=1 X='a && b; $(id)' /usr/bin/claude --agent-id dev@t --agent-name dev --team-name t --lead-verify-key 1763200000000.SECRETKEYSECRETKEY --permission-mode acceptEdits";
    let args = crate::tmux_shim::respawn_args(7, cmdline);
    assert_eq!(args[..3], v(&["respawn", "7", "--teammate"])[..]);
    let line = build_request(&args, Some(3)).unwrap();
    let cmd = parse_request(&line).unwrap().cmd;
    let Cmd::Respawn(rr) = &cmd else {
        panic!("a respawn");
    };
    assert_eq!(rr.target, "7");
    assert_eq!(rr.teammate.as_deref(), Some(cmdline));
    assert_eq!(rr.worktree, None);
    let (action, detail) = cmd.audit_label();
    assert_eq!(action, "respawn");
    assert_eq!(
        detail,
        "target=7 teammate flags=--agent-id,--agent-name,--team-name,--lead-verify-key,\
         --permission-mode unknown=0 env=2"
    );
    assert!(!detail.contains("SECRETKEY") && !detail.contains("my dir") && !detail.contains("id)"));
    // A line that is not the grammar is recorded by its length alone.
    let bad = Cmd::Respawn(RespawnReq {
        target: "7".into(),
        worktree: None,
        teammate: Some("curl evil | sh".into()),
    });
    assert_eq!(
        bad.audit_label().1,
        "target=7 teammate=<unparseable, 14 chars>"
    );
    // A plain respawn is as it was.
    let line = build_request(&v(&["respawn", "7", "--worktree", "w"]), Some(3)).unwrap();
    let Cmd::Respawn(rr) = parse_request(&line).unwrap().cmd else {
        panic!("a respawn");
    };
    assert_eq!((rr.worktree.as_deref(), rr.teammate), (Some("w"), None));
}

#[test]
fn a_teammate_respawn_is_one_line_and_never_a_plain_respawn_by_accident() {
    // The line is read by position: one that spells a flag is still the line.
    let spelled = "cd /d && env claude --worktree w";
    let line = build_request(&v(&["respawn", "7", "--teammate", spelled]), Some(3)).unwrap();
    let Cmd::Respawn(rr) = parse_request(&line).unwrap().cmd else {
        panic!("a respawn");
    };
    assert_eq!((rr.teammate.as_deref(), rr.worktree), (Some(spelled), None));
    for args in [
        v(&["respawn", "7", "--teammate"]),
        // A line never starts with a dash; one that does is a usage error.
        v(&["respawn", "7", "--teammate", "--worktree"]),
        v(&["respawn", "7", "--teammate", "cd /d && env claude", "extra"]),
        v(&["respawn", "7", "--teammate", "x", "--worktree", "w"]),
    ] {
        assert!(build_request(&args, Some(3)).is_err(), "{args:?}");
    }
    // Present but not a string must not fall back to a plain respawn of the
    // target; nor may it ride with a worktree.
    for body in [
        r#""teammate":null"#,
        r#""teammate":true"#,
        r#""teammate":["cd","/d"]"#,
        r#""teammate":"cd /d && env claude","worktree":"w""#,
    ] {
        let line = format!(r#"{{"cmd":"respawn","target":"7",{body}}}"#);
        assert!(parse_request(&line).is_err(), "{line}");
    }
}

/// A teammate respawn names nothing that is atrium's to decide.
#[test]
fn a_teammate_respawn_takes_no_identity_mode_role_command_or_environment() {
    for extra in [
        r#""identity":"prod""#,
        r#""identity":null"#,
        r#""mode":"skip""#,
        r#""role":"boss""#,
        r#""argv":["sh"]"#,
        r#""env":{"ANTHROPIC_BASE_URL":"http://x"}"#,
    ] {
        let line =
            format!(r#"{{"cmd":"respawn","target":"7","teammate":"cd /d && env claude",{extra}}}"#);
        let e = parse_request(&line).unwrap_err();
        assert!(e.contains("a teammate respawn takes no"), "{line}: {e}");
    }
}
