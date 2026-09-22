//! Recognising `atrium up <name>` as `atrium fleet up <name>`.

use super::up_alias;

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn up_name_is_the_alias() {
    // `atrium --trust automode up context-build`: after the leading flags are
    // stripped, `rest` is `["up", "context-build"]` — the alias, launching
    // that fleet with the trust atrium already parsed.
    assert_eq!(
        up_alias(&v(&["up", "context-build"])),
        Some(Ok("context-build"))
    );
}

#[test]
fn non_up_is_not_the_alias() {
    // A real hosted program is left alone (host it as before, never a fleet).
    assert_eq!(up_alias(&v(&["claude", "--model", "opus"])), None);
    assert_eq!(up_alias(&[]), None);
}

#[test]
fn up_without_a_name_is_a_usage_error() {
    assert!(matches!(up_alias(&v(&["up"])), Some(Err(_))));
    // A flag where the name should be is not a name.
    assert!(matches!(up_alias(&v(&["up", "--trust"])), Some(Err(_))));
}

#[test]
fn flags_after_the_name_are_rejected_with_a_pointer() {
    // Flags belong BEFORE `up`; trailing tokens are a usage error, not a
    // silent drop (dropping them is exactly how the posture went missing).
    let args = v(&["up", "context-build", "--allow-ctl"]);
    match up_alias(&args) {
        Some(Err(msg)) => {
            assert!(
                msg.contains("before `up`"),
                "message should redirect: {msg}"
            );
        }
        other => panic!("expected a usage error, got {other:?}"),
    }
}
