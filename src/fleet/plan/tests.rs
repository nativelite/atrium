//! The grant plan: anchors, symlinks, credential stores and what the banner
//! discloses.

use super::*;
use crate::fleet::testutil::s;
use crate::fleet::{parse, real_path, Agent, Fleet, Stores, FILE_NAME};
use std::path::{Path, PathBuf};

/// A real directory tree for one test, canonicalised the way the code
/// canonicalises.
///
/// The canonicalisation is not cosmetic: on macOS `/tmp` is a symlink to
/// `/private/tmp` and on Windows `canonicalize` returns the verbatim
/// `\\?\C:\…` form, so a test comparing a resolved path against a raw
/// `temp_dir()` join asserts something the code never produces. A previous
/// attempt shipped four tests that could not pass on Windows for exactly
/// this reason, and nobody saw it because only `cargo check` runs there.
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("atrium-reach-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(real_path(&p).unwrap())
    }
    /// Create (and canonically name) a directory under the fixture.
    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        real_path(&p).unwrap()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn no_stores() -> Stores {
    Stores::default()
}

fn probe(anchor: &Path, base: &Path, raw: &str) -> Grant {
    Grant::probe(anchor, base, Field::AddDir, raw, &no_stores())
}

/// One agent, one `add_dirs` list — the shape most of these tests need.
fn one_agent_fleet(name: &str, cmd: &str, cwd: Option<&str>, dirs: &[&str]) -> Fleet {
    Fleet {
        grid: None,
        identity: None,
        trust: None,
        allow_ctl: None,
        context: None,
        topics: None,
        worktrees: None,
        worktree_base: None,
        worktree_seed: None,
        build_jobs: None,
        memory_mb: None,
        deny: Vec::new(),
        claude_aliases: Vec::new(),
        agents: vec![Agent {
            name: name.to_string(),
            cmd: vec![cmd.to_string()],
            cwd: cwd.map(str::to_string),
            add_dirs: s(dirs),
            ..Default::default()
        }],
    }
}

#[test]
#[cfg(unix)]
fn a_symlink_out_of_the_tree_is_disclosed_where_it_really_lands() {
    // THE bug that sank the previous attempt: it banned ".." lexically, and
    // its own test showed a symlink walking straight out of the tree and
    // reporting as inside. Classification is made on the resolved path.
    let t = Tmp::new("symout");
    let base = t.dir("proj");
    let secrets = t.dir("secrets");
    link(&secrets, &base.join("looks-local"));
    let g = probe(&base, &base, "./looks-local");
    assert_eq!(
        g.reach,
        Reach::Outside,
        "a symlink out of the tree must not read as inside: {g:?}"
    );
    assert_eq!(g.real.as_deref(), Some(secrets.as_path()));
    assert!(!g.is_quiet());
}

#[test]
#[cfg(unix)]
fn a_symlink_that_stays_in_the_tree_is_still_quiet() {
    // The mirror case. Following symlinks must not turn into "every alias is
    // suspicious" — a banner that shouts at ordinary layouts is one people
    // stop reading.
    let t = Tmp::new("symin");
    let base = t.dir("proj");
    let real = t.dir("proj/real");
    link(&real, &base.join("alias"));
    let g = probe(&base, &base, "./alias");
    assert_eq!(g.reach, Reach::Inside);
    assert!(g.is_quiet(), "an in-tree alias must not be loud: {g:?}");
}

#[test]
#[cfg(unix)]
fn a_symlink_that_cannot_be_followed_is_unverifiable_not_assumed_local() {
    // A dangling link placed at <base>/x would otherwise be "placed" at its
    // own location and reported inside — and the moment its target exists,
    // the grant is wherever that is.
    let t = Tmp::new("dangle");
    let base = t.dir("proj");
    link(Path::new("/no-such-target-atrium"), &base.join("ghost"));
    let g = probe(&base, &base, "./ghost");
    assert_eq!(g.reach, Reach::Unverifiable);
    assert_eq!(g.real, None);
    assert!(!g.is_quiet(), "cannot-tell must never render as inside");
}

#[test]
#[cfg(unix)]
fn dot_dot_is_applied_by_the_filesystem_not_by_string_surgery() {
    // `link/..` pops the TARGET's parent, not the link's. A lexical
    // normaliser gets this exactly backwards and calls an escape "inside".
    let t = Tmp::new("dotdot");
    let base = t.dir("proj");
    let elsewhere = t.dir("elsewhere");
    let inner = t.dir("elsewhere/inner");
    link(&inner, &base.join("link"));
    let g = probe(&base, &base, "./link/..");
    assert_eq!(g.real.as_deref(), Some(elsewhere.as_path()), "got {g:?}");
    assert_eq!(g.reach, Reach::Outside);
}

#[test]
fn a_missing_tail_that_walks_up_is_unverifiable() {
    // Where `<missing>/..` lands depends on a directory that is not there.
    let t = Tmp::new("misstail");
    let base = t.dir("proj");
    let g = probe(&base, &base, "./not-created/..");
    assert_eq!(g.reach, Reach::Unverifiable, "got {g:?}");
}

#[test]
fn a_path_that_does_not_exist_yet_is_placed_but_never_quiet() {
    // atrium does no tilde or env expansion, so `"~/notes"` becomes
    // `<base>/~/notes`: a path that reads to a human as "my home directory"
    // and reaches nothing. Folding it into a terse "N inside" count is a
    // positive false statement about the entry most likely to be wrong.
    let t = Tmp::new("missing");
    let base = t.dir("proj");
    let g = probe(&base, &base, "~/notes");
    assert_eq!(g.reach, Reach::Inside);
    assert!(!g.exists);
    assert!(
        !g.is_quiet(),
        "a path that is not there must be named, not counted"
    );
    assert_eq!(g.tag(), "MISSING");
    assert!(g.note().contains("does not exist"));
}

#[test]
fn the_readme_sibling_checkout_example_is_disclosed_not_refused() {
    // README:214 is `"add_dirs": ["../shared", "./specs"]`. A sibling
    // checkout is a documented, legitimate use; two previous attempts broke
    // this example to close the hole, which is how a control gets switched
    // off. It is shown, not refused.
    let t = Tmp::new("readme");
    let base = t.dir("proj");
    let shared = t.dir("shared");
    let specs = t.dir("proj/specs");
    let g_shared = probe(&base, &base, "../shared");
    let g_specs = probe(&base, &base, "./specs");
    assert_eq!(g_shared.reach, Reach::Outside);
    assert_eq!(g_shared.real.as_deref(), Some(shared.as_path()));
    assert_eq!(g_specs.reach, Reach::Inside);
    assert!(g_specs.is_quiet());
    // And the child still gets a usable path to the sibling: nothing here
    // refuses, drops, or rewrites it into something that is not that dir.
    assert_eq!(g_shared.given, shared);
    assert_eq!(g_specs.given, specs);
}

#[test]
fn the_child_is_handed_the_path_that_was_disclosed() {
    // The banner and the launch must be ONE computation. Resolving a second
    // time in the spawn is how an acknowledged "inside" became a live grant
    // on a credential store when a link was flipped during the Enter window.
    let t = Tmp::new("handed");
    let base = t.dir("proj");
    let real = t.dir("proj/context");
    let g = probe(&base, &base, "./context");
    assert_eq!(g.given, real);
    assert_eq!(Some(g.given.clone()), g.real);
}

#[test]
fn a_credential_store_is_named_even_when_it_is_inside_the_anchor() {
    // The anchor can legitimately be a directory that CONTAINS a credential
    // store (a fleet file kept in $HOME). "Inside" would then be quiet on
    // `./.ssh`, which is the one grant nobody should approve by accident.
    let t = Tmp::new("credin");
    let home = t.dir("home");
    let ssh = t.dir("home/.ssh");
    let stores = Stores::under(&home);
    let g = Grant::probe(&home, &home, Field::AddDir, "./.ssh", &stores);
    assert_eq!(g.reach, Reach::Inside);
    assert_eq!(g.real.as_deref(), Some(ssh.as_path()));
    assert!(
        !g.is_quiet(),
        "a credential store must never be counted quietly"
    );
    assert_eq!(g.tag(), "CREDENTIALS");
    assert!(g.note().contains("SSH private keys"), "{:?}", g.note());
}

#[test]
fn granting_a_parent_of_a_credential_store_says_what_it_encloses() {
    // `--add-dir $HOME` hands over ~/.ssh exactly as surely as naming it,
    // and that is the case an operator is least likely to work out from the
    // path alone.
    let t = Tmp::new("credup");
    let home = t.dir("home");
    t.dir("home/.ssh");
    let stores = Stores::under(&home);
    let g = Grant::probe(&t.0, &t.0, Field::AddDir, "./home", &stores);
    assert!(!g.is_quiet());
    assert!(
        g.note().contains("encloses") && g.note().contains("SSH private keys"),
        "{:?}",
        g.note()
    );
}

#[test]
fn a_credential_store_that_is_not_installed_is_never_cited() {
    // An earlier attempt refused `$HOME/.config` because it "encloses
    // .config/gcloud, which holds Google Cloud credentials" on a machine
    // with no gcloud installed. A warning the operator can see is wrong is a
    // warning they learn to skip.
    let t = Tmp::new("nostore");
    let home = t.dir("home");
    t.dir("home/.ssh");
    let cfg = t.dir("home/.config");
    let stores = Stores::under(&home);
    assert_eq!(stores.describe(Some(&cfg)), None);
}

#[test]
fn the_anchor_is_the_repo_the_fleet_file_is_checked_into() {
    // Measured failure of anchoring on the fleet file's own directory: a
    // monorepo whose file lives in `tools/` marked every same-repo path
    // OUTSIDE — 18 loud lines for a 6-agent roster, scrolling the trust
    // posture off a 24-row terminal.
    let t = Tmp::new("anchorrepo");
    let repo = t.dir("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let tools = t.dir("repo/tools");
    let api = t.dir("repo/packages/api");
    let a = anchor_for(&tools, &tools, false, None);
    assert_eq!(a.kind, AnchorKind::Repo);
    assert_eq!(a.dir, repo);
    let g = probe(&a.dir, &tools, "../packages/api");
    assert_eq!(g.reach, Reach::Inside, "same-repo path must not be OUTSIDE");
    assert!(g.is_quiet());
    assert_eq!(g.given, api);
}

#[test]
fn the_global_fleet_file_is_anchored_at_the_directory_you_ran_in() {
    // `~/.config/atrium/fleet.json`'s own directory holds no project by
    // construction, so anchoring there makes 100% of paths OUTSIDE — a tag
    // on every line carries no information at all.
    let t = Tmp::new("anchorglobal");
    let cfgdir = t.dir("cfg/atrium");
    let work = t.dir("work");
    let a = anchor_for(&cfgdir, &work, true, None);
    assert_eq!(a.kind, AnchorKind::InvokingDir);
    assert_eq!(a.dir, work);
}

#[test]
fn the_anchor_never_swallows_the_home_directory() {
    // A home directory that happens to be a git repo would make "inside"
    // cover ~/.ssh, i.e. mean nothing.
    let t = Tmp::new("anchorhome");
    let home = t.dir("home");
    std::fs::create_dir_all(home.join(".git")).unwrap();
    let proj = t.dir("home/proj");
    let a = anchor_for(&proj, &proj, false, Some(&home));
    assert_eq!(a.kind, AnchorKind::FleetDir);
    assert_eq!(a.dir, proj);
}

#[test]
fn identical_grants_across_agents_are_disclosed_once() {
    // Measured: ten agents each carrying the documented
    // `["../shared","../protos"]` printed twenty near-identical loud lines
    // for two facts, wrapped on 80 columns, and pushed the posture and
    // can-spawn lines off the screen the Enter was asked on.
    let t = Tmp::new("dedupe");
    let base = t.dir("proj");
    t.dir("shared");
    t.dir("protos");
    let agents: Vec<Agent> = (0..10)
        .map(|i| Agent {
            name: format!("a{i}"),
            cmd: s(&["claude"]),
            add_dirs: s(&["../shared", "../protos"]),
            ..Default::default()
        })
        .collect();
    let fleet = Fleet {
        grid: None,
        identity: None,
        trust: None,
        allow_ctl: None,
        context: None,
        topics: None,
        worktrees: None,
        worktree_base: None,
        worktree_seed: None,
        build_jobs: None,
        memory_mb: None,
        deny: Vec::new(),
        claude_aliases: Vec::new(),
        agents,
    };
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    let lines = plan.banner_lines();
    let loud: Vec<&String> = lines.iter().filter(|l| l.contains("OUTSIDE")).collect();
    assert_eq!(
        loud.len(),
        2,
        "one line per directory, not per agent: {lines:#?}"
    );
    assert!(
        loud[0].contains("a0") && loud[0].contains("+7"),
        "{}",
        loud[0]
    );
    // And the whole banner stays inside a terminal's worth of rows.
    assert!(lines.len() <= 8, "banner too tall: {lines:#?}");
}

#[test]
fn the_verdict_names_the_destinations_not_just_a_count() {
    // The banner scrolls; the line printed immediately before the blocking
    // Enter does not. A surviving summary that reports a bare count is the
    // one line an operator is guaranteed to read and the one line that tells
    // them nothing.
    let t = Tmp::new("verdict");
    let base = t.dir("proj");
    let shared = t.dir("shared");
    let fleet = one_agent_fleet("lead", "claude", None, &["../shared", "./in"]);
    t.dir("proj/in");
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    let v = plan.verdict();
    assert!(v.contains("GRANTS 1"), "{v}");
    assert!(
        v.contains(shared.file_name().unwrap().to_str().unwrap()),
        "the verdict must name where the grant goes: {v}"
    );
}

#[test]
fn a_fleet_with_nothing_to_disclose_says_so_plainly() {
    let t = Tmp::new("clean");
    let base = t.dir("proj");
    t.dir("proj/specs");
    let fleet = one_agent_fleet("lead", "claude", None, &["./specs"]);
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    assert!(plan
        .verdict()
        .starts_with("every agent dir resolves inside"));
    assert!(!plan.banner_lines().iter().any(|l| l.contains("OUTSIDE")));
}

#[test]
fn an_agent_that_does_not_run_an_agent_cli_is_disclosed_as_opaque() {
    // `["sh","-c","claude --add-dir ~/.ssh"]` grants read access to the SSH
    // keys through a channel atrium does not parse and should not: the direct
    // `--add-dir` in `cmd` IS refused by vet_spawn_argv, which makes the
    // shell wrapper a trap rather than an obscure edge. atrium cannot close
    // it, so it says so on the banner instead of implying completeness.
    let t = Tmp::new("opaque");
    let base = t.dir("proj");
    let mut fleet = one_agent_fleet("one", "sh", None, &[]);
    fleet.agents.push(Agent {
        name: "two".to_string(),
        cmd: s(&["claude"]),
        ..Default::default()
    });
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    assert_eq!(plan.agents[0].opaque.as_deref(), Some("sh"));
    assert_eq!(plan.agents[1].opaque, None);
    assert!(plan
        .banner_lines()
        .iter()
        .any(|l| l.contains("do not run an agent CLI") && l.contains("one")));
}

#[test]
fn no_file_supplied_string_can_inject_an_escape_into_the_banner() {
    // Every one of these arrives through the `json` parser, which decodes
    // `\u001b` into a real ESC.
    let t = Tmp::new("inject");
    let base = t.dir("proj");
    let text = r#"{"fleets":{"f":{"identity":"\u001b[31mid",
          "agents":[{"name":"ev\u001b[2J\u001b[Hil","cmd":["claude"],
          "add_dirs":["./\u001b[2Ktrick","\u202egnp.txt"],
          "identity":"\u001bbad"}]}}}"#;
    let fleets = parse(text).unwrap();
    let fleet = fleets.get("f").unwrap();
    assert!(
        fleet.agents[0].name.contains('\u{1b}'),
        "the parser really does decode an escape - otherwise this test proves nothing"
    );
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    let mut all = plan.banner_lines();
    all.push(plan.verdict());
    all.push(plan.agents[0].label.clone());
    for line in &all {
        assert!(
            !line.chars().any(|c| c.is_control()),
            "raw control character reached the banner: {line:?}"
        );
        assert!(
            !line.contains('\u{202e}'),
            "bidi override reached the banner: {line:?}"
        );
    }
    assert!(all.iter().any(|l| l.contains("\\u{1b}")), "{all:#?}");
}

#[test]
fn one_huge_path_cannot_scroll_the_rest_of_the_disclosure_away() {
    let t = Tmp::new("flood");
    let base = t.dir("proj");
    let huge = format!("./{}", "a".repeat(40_000));
    let fleet = one_agent_fleet("lead", "claude", None, &[&huge, "../out"]);
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &no_stores());
    for line in plan.banner_lines() {
        assert!(
            line.chars().count() < 600,
            "banner line runaway: {}",
            line.len()
        );
    }
}

#[test]
fn a_credential_store_cannot_be_pushed_off_the_banner_by_noise() {
    // The banner is capped, so a roster could otherwise bury
    // `--add-dir ~/.ssh` behind a wall of innocuous outside directories and
    // push it past the cut - the same trick as burying a flag in `cmd` and
    // hoping the review skims.
    let t = Tmp::new("bury");
    let base = t.dir("proj");
    let home = t.dir("home");
    t.dir("home/.ssh");
    let stores = Stores::under(&home);
    let mut dirs: Vec<String> = (0..20)
        .map(|i| {
            t.dir(&format!("noise{i}"));
            format!("../noise{i}")
        })
        .collect();
    dirs.push("../home/.ssh".to_string());
    let refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let fleet = one_agent_fleet("lead", "claude", None, &refs);
    let anchor = Anchor {
        dir: base.clone(),
        kind: AnchorKind::FleetDir,
    };
    let plan = Plan::build(&fleet, &base.join(FILE_NAME), &base, anchor, &stores);
    let lines = plan.banner_lines();
    assert!(
        lines[1].contains("CREDENTIALS") && lines[1].contains("SSH private keys"),
        "the worst grant must be the first one printed: {lines:#?}"
    );
    assert!(
        plan.verdict().contains(".ssh"),
        "and it must be in the line the prompt sits under: {}",
        plan.verdict()
    );
}

#[test]
fn a_sibling_whose_name_merely_starts_with_the_anchor_is_outside() {
    // `/repo_secrets` is not inside `/repo`. A string `starts_with` says it
    // is; `Path::starts_with` is component-wise and does not.
    let t = Tmp::new("prefix");
    let base = t.dir("proj");
    let sneaky = t.dir("proj_secrets");
    let g = probe(&base, &t.0, "./proj_secrets");
    assert_eq!(g.reach, Reach::Outside, "got {g:?}");
    assert_eq!(g.real.as_deref(), Some(sneaky.as_path()));
}

#[test]
fn an_absolute_path_inside_the_anchor_is_still_inside() {
    // Including through a symlinked route: both sides are canonicalised, so
    // macOS's `/tmp -> /private/tmp` is not a false alarm.
    let t = Tmp::new("abs");
    let base = t.dir("proj");
    let inner = t.dir("proj/specs");
    let g = probe(&base, &base, inner.to_str().unwrap());
    assert_eq!(g.reach, Reach::Inside);
    assert!(g.is_quiet());
}

// --- context block -------------------------------------------------------
