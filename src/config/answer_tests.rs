//! The answer to the mod offer: the one-key config edit and the answer file.

use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atrium-answer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A config a person formatted by hand: tabs, a CRLF, odd spacing, a nested
/// object and a string that both spell `"mod"`.
const HAND: &str = "{\r\n\t\"deny\" : [ \"Bash(rm -rf *)\" ],\n\n   \"fleet_defaults\": { \"trust\": \"automode\", \"grid\":null },\n  \"commands\": [\"/compact\"],   \"notify\":\"bell\"\n}\n";

#[test]
fn true_is_a_yes_false_is_off_and_null_says_nothing() {
    let yes = parse(r#"{"mod": true}"#).unwrap();
    assert!(yes.mod_yes && !yes.mod_off && yes.mod_said());
    let no = parse(r#"{"mod": false}"#).unwrap();
    assert!(no.mod_off && !no.mod_yes && no.mod_said());
    let path = parse(r#"{"mod": {"path": "/m"}}"#).unwrap();
    assert!(path.mod_said() && !path.mod_yes && !path.mod_off);
    for silent in [r#"{"mod": null}"#, "{}"] {
        assert!(!parse(silent).unwrap().mod_said(), "{silent}");
    }
    // The starter says nothing, so a new user is asked.
    assert!(!parse(STARTER).unwrap().mod_said());
}

#[test]
fn an_absent_key_is_added_first_and_every_other_byte_stays() {
    let new = set_mod_key(HAND, true).unwrap();
    assert_eq!(
        new,
        HAND.replacen("{\r\n\t", "{\r\n\t\"mod\": true,\r\n\t", 1)
    );
    // Taking the one member back out gives the file as it was.
    assert_eq!(new.replacen("\"mod\": true,\r\n\t", "", 1), HAND);
    let cfg = parse(&new).unwrap();
    assert!(cfg.mod_yes);
    assert_eq!(cfg.deny, ["Bash(rm -rf *)"]);
    assert_eq!(cfg.fleet_defaults.trust.as_deref(), Some("automode"));

    assert_eq!(
        set_mod_key(r#"{"deny":[]}"#, false).unwrap(),
        r#"{"mod": false, "deny":[]}"#
    );
    assert_eq!(set_mod_key("{}", true).unwrap(), r#"{"mod": true}"#);
    assert_eq!(
        set_mod_key(" {\n}\n", false).unwrap(),
        " {\"mod\": false\n}\n"
    );
}

#[test]
fn a_key_that_is_there_has_only_its_value_replaced() {
    for (was, yes, want) in [
        (
            "{\n  \"a\": 1,\n  \"mod\" :  null ,\n  \"b\": 2\n}",
            true,
            "{\n  \"a\": 1,\n  \"mod\" :  true ,\n  \"b\": 2\n}",
        ),
        (r#"{"mod":false}"#, true, r#"{"mod":true}"#),
        (r#"{"mod":true,"x":"y"}"#, false, r#"{"mod":false,"x":"y"}"#),
        // The starter: `"mod": null` becomes the answer, in place.
        (
            STARTER,
            false,
            &STARTER.replace("\"mod\": null", "\"mod\": false"),
        ),
    ] {
        assert_eq!(set_mod_key(was, yes).as_deref(), Some(want), "{was}");
    }
    // The last one is the one `parse` reads, so it is the one changed.
    assert_eq!(
        set_mod_key(r#"{"mod": true, "mod": null}"#, false).unwrap(),
        r#"{"mod": true, "mod": false}"#
    );
}

#[test]
fn only_the_top_level_key_is_the_key() {
    // A nested `"mod"` and a string that spells one are not it.
    let text = r#"{"fleet_defaults": {"mod": 1}, "deny": ["\"mod\": false", "a\\"], "z": [{"mod": null}]}"#;
    let new = set_mod_key(text, true).unwrap();
    assert_eq!(new, text.replacen('{', "{\"mod\": true, ", 1));
}

#[test]
fn text_the_scanner_cannot_walk_is_left_alone() {
    for bad in [
        "",
        "[]",
        "\"mod\"",
        "{\"a\": 1",
        "{\"a\" 1}",
        "{a: 1}",
        "{\"a\": 1} trailing",
        "{\"a\": \"unterminated}",
    ] {
        assert_eq!(set_mod_key(bad, true), None, "{bad:?}");
    }
}

#[test]
fn the_answer_goes_into_an_existing_config_and_touches_nothing_else() {
    let d = temp("config");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    std::fs::write(&config, HAND).unwrap();
    // A leftover answer file: the config's key is now the one place.
    std::fs::write(&file, "yes\n").unwrap();

    let kept = record_mod_answer_at(Some(&config), Some(&file), false).unwrap();
    assert_eq!(kept, Kept::Config(config.clone()));
    let text = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        text,
        HAND.replacen("{\r\n\t", "{\r\n\t\"mod\": false,\r\n\t", 1)
    );
    assert!(!file.exists(), "one place holds the answer");
    assert_eq!(
        mod_answer_kept_at(Some(&config), Some(&file)),
        Some((false, Kept::Config(config.clone())))
    );
    assert!(kept.describe(false).contains("\"mod\": false"));

    // Changing one's mind changes the same key back, and nothing else.
    record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        text,
        HAND.replacen("{\r\n\t", "{\r\n\t\"mod\": true,\r\n\t", 1)
    );

    // The same answer again writes nothing at all.
    let before = std::fs::metadata(&config).unwrap().modified().unwrap();
    record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert_eq!(
        std::fs::metadata(&config).unwrap().modified().unwrap(),
        before
    );
    // No temporary file is left beside it.
    let names: Vec<String> = std::fs::read_dir(&d)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["config.json"], "{names:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_config_that_does_not_parse_is_never_written_to() {
    let d = temp("broken");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    for broken in ["{ \"deny\": [ ", "{\"notify\": \"loudly\"}", "not json"] {
        std::fs::write(&config, broken).unwrap();
        let kept = record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
        assert_eq!(kept, Kept::File(file.clone()), "{broken}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), broken);
        assert_eq!(read_mod_answer_file(&file), Some(true));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_config_that_names_its_own_folder_is_left_as_it_is() {
    let d = temp("managed");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    let text = r#"{"mod": {"path": "/somewhere/mod"}}"#;
    std::fs::write(&config, text).unwrap();
    let err = record_mod_answer_at(Some(&config), Some(&file), true).unwrap_err();
    assert!(err.contains("names its own"), "{err}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert!(!file.exists());
    assert_eq!(mod_answer_kept_at(Some(&config), Some(&file)), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn with_no_config_file_the_answer_is_kept_in_the_answer_file() {
    let d = temp("file");
    let (config, file) = (d.join("config.json"), d.join("sub").join("mod.answer"));
    assert_eq!(mod_answer_kept_at(Some(&config), Some(&file)), None);
    for yes in [false, true] {
        let kept = record_mod_answer_at(Some(&config), Some(&file), yes).unwrap();
        assert_eq!(kept, Kept::File(file.clone()));
        assert!(!config.exists(), "no config is made to hold an answer");
        assert_eq!(read_mod_answer_file(&file), Some(yes));
        assert_eq!(
            mod_answer_kept_at(Some(&config), Some(&file)),
            Some((yes, Kept::File(file.clone())))
        );
    }
    // Only the two words are an answer.
    for junk in ["", "maybe\n", "true\n", "Yes\n", "\nyes\n"] {
        std::fs::write(&file, junk).unwrap();
        assert_eq!(read_mod_answer_file(&file), None, "{junk:?}");
    }
    // Nowhere to keep it is an error, not a silent loss.
    assert!(record_mod_answer_at(None, None, true).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_configs_key_always_wins_over_the_answer_file() {
    let fold = |text: &str, file: Option<bool>| {
        let mut cfg = parse(text).unwrap();
        cfg.fold_mod_answer(file);
        (cfg.mod_yes, cfg.mod_off)
    };
    // The file speaks only where the config says nothing.
    assert_eq!(fold("{}", Some(true)), (true, false));
    assert_eq!(fold("{}", Some(false)), (false, true));
    assert_eq!(fold("{}", None), (false, false));
    assert_eq!(fold(r#"{"mod": null}"#, Some(false)), (false, true));
    // The config's own key wins.
    assert_eq!(fold(r#"{"mod": false}"#, Some(true)), (false, true));
    assert_eq!(fold(r#"{"mod": true}"#, Some(false)), (true, false));
    assert_eq!(
        fold(r#"{"mod": {"path": "/m"}}"#, Some(false)),
        (false, false)
    );

    let d = temp("wins");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    std::fs::write(&config, r#"{"mod": false}"#).unwrap();
    std::fs::write(&file, "yes\n").unwrap();
    assert_eq!(
        mod_answer_kept_at(Some(&config), Some(&file)),
        Some((false, Kept::Config(config.clone())))
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A recorded no is `"mod": false` for a fleet too: off, as today.
#[test]
fn a_no_from_the_answer_file_turns_the_mod_off_for_a_fleet() {
    let fleet = || {
        crate::fleet::parse(r#"{"fleets":{"f":{"agents":[{"name":"a","cmd":["claude"]}]}}}"#)
            .unwrap()
            .get("f")
            .unwrap()
            .clone()
    };
    let mut cfg = GlobalConfig::default();
    cfg.fold_mod_answer(Some(false));
    assert_eq!(cfg.apply_to_fleet(fleet()).mod_enabled, Some(false));
    let mut cfg = GlobalConfig::default();
    cfg.fold_mod_answer(Some(true));
    assert_eq!(cfg.apply_to_fleet(fleet()).mod_enabled, None);
}
