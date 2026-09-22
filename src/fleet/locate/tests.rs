//! Locating the fleet file and resolving directories against it.

use super::*;
use crate::fleet::parse;
use std::path::{Path, PathBuf};

#[test]
fn resolve_dir_joins_relative_onto_base() {
    let base = Path::new("/proj");
    assert_eq!(resolve_dir(base, "review"), PathBuf::from("/proj/review"));
    assert_eq!(
        resolve_dir(base, "./sub"),
        PathBuf::from("/proj/./sub"),
        "relative kept as a join (normalization is the OS's job)"
    );
}

#[test]
fn resolve_dir_keeps_absolute_as_is() {
    let base = Path::new("/proj");
    #[cfg(not(windows))]
    assert_eq!(resolve_dir(base, "/etc/x"), PathBuf::from("/etc/x"));
    #[cfg(windows)]
    assert_eq!(resolve_dir(base, "C:\\etc\\x"), PathBuf::from("C:\\etc\\x"));
}

// --- discovery -----------------------------------------------------------

#[test]
fn discover_finds_the_local_file_first() {
    let td = std::env::temp_dir().join(format!("atrium-fleet-disc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    std::fs::write(td.join(FILE_NAME), "{}").unwrap();
    let found = discover(&td).unwrap();
    assert_eq!(found.path, td.join(FILE_NAME));
    assert_eq!(found.dir, td);
    let _ = std::fs::remove_dir_all(&td);
}

/// Serializes the tests that move `XDG_CONFIG_HOME`, since the environment
/// is process-wide and the suite runs in parallel.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A user's template is copied as written: the object's text, braces and
/// quotes inside prompts included, not a re-serialization.
#[test]
fn a_fleets_object_text_is_extracted_verbatim() {
    let file = r#"{
  "fleets": {
    "other": { "agents": [ { "name": "x", "cmd": ["claude"], "prompt": "has { braces } and \"quotes\"" } ] },
    "mine":  {
      "grid": "2x2",
      "agents": [ { "name": "a", "cmd": ["claude"], "prompt": "say \"}\" not }" } ]
    }
  }
}"#;
    let got = fleet_object_text(file, "mine").unwrap();
    assert!(got.starts_with("{\n      \"grid\": \"2x2\""), "{got}");
    assert!(got.ends_with("]\n    }"), "{got}");
    assert!(got.contains(r#""prompt": "say \"}\" not }""#), "{got}");
    let other = fleet_object_text(file, "other").unwrap();
    assert!(other.contains("has { braces }"), "{other}");
    assert_eq!(fleet_object_text(file, "nope"), None);
    // What was extracted is itself a valid fleet object.
    let wrapped = format!("{{\"fleets\":{{\"mine\":{got}}}}}");
    assert!(parse(&wrapped).is_ok());
}

#[test]
fn discover_missing_names_both_locations() {
    // Point the *global* location at an empty dir for the duration. Without
    // this the test reads the developer's real `~/.config/atrium/fleet.json`
    // and fails the moment they have one — which is a supported, documented
    // setup, so the test was asserting "no global fleet file exists on this
    // machine" rather than the behaviour it means to cover.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // BOTH variables, not just the unix one. `global_path` reads APPDATA on
    // Windows and XDG_CONFIG_HOME elsewhere, so overriding only XDG left this
    // test reading the developer's real %APPDATA%\atrium\fleet.json — it was
    // hermetic on exactly one platform, which is the same class of bug the
    // previous fix here was meant to close.
    let prev_appdata = std::env::var_os("APPDATA");
    let prev = std::env::var_os("XDG_CONFIG_HOME");
    let empty = std::env::temp_dir().join(format!("atrium-fleet-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty);
    std::fs::create_dir_all(&empty).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &empty);
    std::env::set_var("APPDATA", &empty);

    let td = std::env::temp_dir().join(format!("atrium-fleet-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&td);
    std::fs::create_dir_all(&td).unwrap();
    let found = discover(&td);

    match prev {
        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match prev_appdata {
        Some(v) => std::env::set_var("APPDATA", v),
        None => std::env::remove_var("APPDATA"),
    }
    let _ = std::fs::remove_dir_all(&empty);

    let err = found.unwrap_err();
    assert!(err.contains(FILE_NAME), "names local: {err}");
    // Names the global location too (the word "then" separates the two).
    assert!(err.contains("then"), "names both: {err}");
    let _ = std::fs::remove_dir_all(&td);
}

// --- disclosure: where a fleet file's grants actually land ---------------
