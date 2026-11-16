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
    assert!(path.mod_said() && path.mod_managed && !path.mod_yes && !path.mod_off);
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
    assert!(err.contains("has its own \"mod\" object"), "{err}");
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

/// An object-valued `"mod"` is the user's, whatever is in it: an empty one,
/// an empty path, a mistyped key. Unknown keys are let be, as everywhere in
/// this file; the object is never replaced, and nobody is asked over it.
#[test]
fn an_object_valued_mod_is_the_users_and_is_never_replaced() {
    let d = temp("object");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    for text in [
        r#"{"mod": {}}"#,
        r#"{"mod": {"path": ""}}"#,
        r#"{"mod": {"paht": "/typo/mod"}}"#,
        "{\n  \"mod\": {\n    \"path\": \"/m\",\n    \"note\": \"mine\"\n  }\n}\n",
    ] {
        let cfg = parse(text).unwrap();
        assert!(cfg.mod_managed && cfg.mod_said(), "{text}");
        assert!(!cfg.mod_yes && !cfg.mod_off, "{text}");
        for yes in [true, false] {
            assert_eq!(set_mod_key(text, yes), None, "{text}");
            std::fs::write(&config, text).unwrap();
            let err = record_mod_answer_at(Some(&config), Some(&file), yes).unwrap_err();
            assert!(err.contains("left as it is"), "{err}");
            assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
            assert!(!file.exists(), "and no answer file is written around it");
        }
        assert_eq!(mod_answer_target_at(Some(&config), Some(&file)), None);
        // The answer file never speaks over it either.
        let mut cfg = parse(text).unwrap();
        cfg.fold_mod_answer(Some(false));
        assert!(!cfg.mod_off, "{text}");
    }
    // Only null, true and false are replaced; anything else is not ours.
    for text in [r#"{"mod": "yes"}"#, r#"{"mod": 1}"#, r#"{"mod": []}"#] {
        assert_eq!(set_mod_key(text, true), None, "{text}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Where the answer will go is known before the question is asked.
#[test]
fn where_the_answer_will_be_kept_is_known_beforehand() {
    let d = temp("target");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    assert_eq!(
        mod_answer_target_at(Some(&config), Some(&file)),
        Some(Kept::File(file.clone()))
    );
    std::fs::write(&config, "{}").unwrap();
    let target = mod_answer_target_at(Some(&config), Some(&file)).unwrap();
    assert_eq!(target, Kept::Config(config.clone()));
    assert!(
        target.place().contains("its \"mod\" key"),
        "{}",
        target.place()
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "{}",
        "nothing is written"
    );
    std::fs::write(&config, "not json").unwrap();
    assert_eq!(
        mod_answer_target_at(Some(&config), Some(&file)),
        Some(Kept::File(file.clone()))
    );
    assert_eq!(mod_answer_target_at(None, None), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// A read-only config is someone saying hands off: it is left untouched and
/// the answer goes beside it. When its own key says the opposite, that is an
/// error naming what is in the way.
#[test]
fn a_read_only_config_is_left_untouched_and_the_answer_goes_beside_it() {
    let d = temp("readonly");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    let lock = |on: bool| {
        let mut perms = std::fs::metadata(&config).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(if on { 0o444 } else { 0o644 });
        }
        #[cfg(not(unix))]
        perms.set_readonly(on);
        std::fs::set_permissions(&config, perms).unwrap();
    };
    std::fs::write(&config, "{ \"deny\": [] }").unwrap();
    lock(true);
    let kept = record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
    assert_eq!(kept, Kept::File(file.clone()));
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "{ \"deny\": [] }"
    );
    assert!(std::fs::metadata(&config).unwrap().permissions().readonly());
    assert_eq!(read_mod_answer_file(&file), Some(true));

    lock(false);
    std::fs::write(&config, r#"{"mod": false}"#).unwrap();
    lock(true);
    let err = record_mod_answer_at(Some(&config), Some(&file), true).unwrap_err();
    assert!(
        err.contains("is read-only") && err.contains("by hand"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        r#"{"mod": false}"#
    );
    lock(false);
    let _ = std::fs::remove_dir_all(&d);
}

/// On unix the edited file is a new file under the old name: it keeps the
/// old one's mode, and a file with a second name (a hard link) is not
/// replaced, since the other name would keep the old text.
#[cfg(unix)]
#[test]
fn the_edited_config_keeps_its_mode_and_a_hard_linked_one_is_not_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let d = temp("mode");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    for private in [0o600, 0o640, 0o664] {
        std::fs::write(&config, "{}").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(private)).unwrap();
        let kept = record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
        assert_eq!(kept, Kept::Config(config.clone()));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            r#"{"mod": true}"#
        );
        assert_eq!(mode(&config), private, "{private:o} stays {private:o}");
    }
    // 0444: left untouched, the answer beside it.
    std::fs::write(&config, "{}").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444)).unwrap();
    let kept = record_mod_answer_at(Some(&config), Some(&file), false).unwrap();
    assert_eq!(kept, Kept::File(file.clone()));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "{}");
    assert_eq!(mode(&config), 0o444);
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = std::fs::remove_file(&file);

    // A second hard link: not replaced.
    let other = d.join("other-name.json");
    std::fs::hard_link(&config, &other).unwrap();
    let kept = record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
    assert_eq!(kept, Kept::File(file.clone()));
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "{}");
    std::fs::remove_file(&other).unwrap();
    let _ = std::fs::remove_file(&file);

    // A symbolic link: not written through, and the message says what it is.
    let real = d.join("real.json");
    std::fs::rename(&config, &real).unwrap();
    std::os::unix::fs::symlink(&real, &config).unwrap();
    let kept = record_mod_answer_at(Some(&config), Some(&file), true).unwrap();
    assert_eq!(kept, Kept::File(file.clone()));
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "{}");
    std::fs::write(&real, r#"{"mod": false}"#).unwrap();
    let err = record_mod_answer_at(Some(&config), Some(&file), true).unwrap_err();
    assert!(err.contains("is a symbolic link"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Temporary files an interrupted write left beside the config are swept
/// once they are old; a fresh one (another atrium mid-write) and any other
/// file are left alone.
#[test]
fn stale_temporaries_beside_the_config_are_swept() {
    let d = temp("sweep");
    let (config, file) = (d.join("config.json"), d.join("mod.answer"));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let aged = |name: &str, age: Option<std::time::SystemTime>| {
        let p = d.join(name);
        std::fs::write(&p, "x").unwrap();
        if let Some(at) = age {
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        p
    };
    let gone = [
        aged("config.json.4242.atrium-tmp", Some(old)),
        aged("mod.answer.17.atrium-tmp", Some(old)),
    ];
    let stay = [
        aged("config.json.4243.atrium-tmp", None),
        aged("config.json", Some(old)),
        aged("config.json.bak.atrium-tmp", Some(old)),
        aged("config.json..atrium-tmp", Some(old)),
        aged("other.json.4242.atrium-tmp", Some(old)),
        aged("config.json.4242.atrium-tmp.keep", Some(old)),
    ];
    sweep_temporaries(&[&config, &file]);
    for p in &gone {
        assert!(!p.exists(), "{}", p.display());
    }
    for p in &stay {
        assert!(p.exists(), "{}", p.display());
    }
    let _ = std::fs::remove_dir_all(&d);
}
