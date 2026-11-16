//! The first-run offer, what a yes covers at launch, the line for nobody
//! there, and which path a named teammate takes (PLAN 0.48.0, S3).

use super::*;
use crate::modstate::Subagents;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atrium-offer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A folder as an atrium before 0.48 left it: the files, no marker, and an
/// older version in the manifest.
fn old_install(dir: &Path, version: &str) {
    install_at(dir).unwrap();
    std::fs::remove_file(dir.join(MARKER)).unwrap();
    std::fs::write(
        dir.join(MANIFEST),
        format!("{{\"name\":\"atrium\",\"version\":\"{version}\"}}"),
    )
    .unwrap();
}

const ASKED: OfferFacts = OfferFacts {
    human: true,
    env_named: false,
    said: Said::Unset,
    folder: Some(Folder::Absent),
    checkout: false,
    claude: true,
};

/// Who is asked: a person, no marked install, nothing in the config or the
/// environment that manages the folder, claude on PATH, no answer yet.
#[test]
fn the_decision_table_of_who_is_asked() {
    use NotAsked::*;
    assert_eq!(offer_of(&ASKED), Offer::Install);
    // A mod an older atrium left without a marker: asked, as an adoption.
    assert_eq!(
        offer_of(&OfferFacts {
            folder: Some(Folder::Unmarked),
            ..ASKED
        }),
        Offer::Adopt
    );
    let cases: &[(OfferFacts, NotAsked)] = &[
        (
            OfferFacts {
                human: false,
                ..ASKED
            },
            NobodyThere,
        ),
        (
            OfferFacts {
                env_named: true,
                ..ASKED
            },
            EnvNamed,
        ),
        (
            OfferFacts {
                said: Said::Managed,
                ..ASKED
            },
            Managed,
        ),
        (
            OfferFacts {
                said: Said::No,
                ..ASKED
            },
            SaidNo,
        ),
        (
            OfferFacts {
                said: Said::Yes,
                ..ASKED
            },
            SaidYes,
        ),
        (
            OfferFacts {
                folder: None,
                ..ASKED
            },
            NoFolder,
        ),
        (
            OfferFacts {
                folder: Some(Folder::Ours),
                ..ASKED
            },
            Installed,
        ),
        (
            OfferFacts {
                checkout: true,
                ..ASKED
            },
            Checkout,
        ),
        (
            OfferFacts {
                claude: false,
                ..ASKED
            },
            NoClaude,
        ),
    ];
    for (facts, why) in cases {
        assert_eq!(offer_of(facts), Offer::Not(*why), "{facts:?}");
    }
    // Every combination: nobody is asked unless every condition holds, and
    // where nobody is there nothing else is even looked at.
    let saids = [Said::Unset, Said::Yes, Said::No, Said::Managed];
    let folders = [
        None,
        Some(Folder::Absent),
        Some(Folder::Unmarked),
        Some(Folder::Ours),
    ];
    for bits in 0u8..16 {
        for said in saids {
            for folder in folders {
                let f = OfferFacts {
                    human: bits & 1 != 0,
                    env_named: bits & 2 != 0,
                    checkout: bits & 4 != 0,
                    claude: bits & 8 != 0,
                    said,
                    folder,
                };
                let asks = f.human
                    && !f.env_named
                    && !f.checkout
                    && f.claude
                    && said == Said::Unset
                    && matches!(folder, Some(Folder::Absent | Folder::Unmarked));
                assert_eq!(!matches!(offer_of(&f), Offer::Not(_)), asks, "{f:?}");
                if !f.human {
                    assert_eq!(offer_of(&f), Offer::Not(NobodyThere), "{f:?}");
                }
            }
        }
    }
}

/// A person is there only with a terminal on stdin, no `ATRIUM_YES`, and
/// not inside an atrium pane, whose stdin is a terminal an agent may type
/// into.
#[test]
fn nobody_is_there_without_a_terminal_under_atrium_yes_or_inside_a_pane() {
    assert!(person_there(false, true, true, None));
    assert!(
        person_there(false, true, true, Some("")),
        "empty counts as unset"
    );
    assert!(!person_there(true, true, true, None), "ATRIUM_YES");
    assert!(
        !person_there(false, false, true, None),
        "no terminal on stdin"
    );
    // The question is written to stderr: redirected, nobody would see it.
    assert!(
        !person_there(false, true, false, None),
        "stderr is not a terminal"
    );
    assert!(!person_there(false, true, true, Some("3")), "inside a pane");
    assert!(!person_there(true, false, false, Some("3")));
}

#[test]
fn what_the_config_says_of_the_mod() {
    let said = |text: &str| Said::of(&crate::config::parse(text).unwrap());
    assert_eq!(said("{}"), Said::Unset);
    assert_eq!(said(r#"{"mod": null}"#), Said::Unset);
    assert_eq!(said(r#"{"mod": true}"#), Said::Yes);
    assert_eq!(said(r#"{"mod": false}"#), Said::No);
    assert_eq!(said(r#"{"mod": {"path": "/m"}}"#), Said::Managed);
    // The answer file, folded in, reads the same way.
    let mut cfg = crate::config::GlobalConfig::default();
    cfg.fold_mod_answer(Some(false));
    assert_eq!(Said::of(&cfg), Said::No);
}

#[test]
fn enter_is_yes_and_only_the_plain_words_are_an_answer() {
    for yes in ["", "\n", "\r\n", "y", "Y\n", "yes", " YES \r\n"] {
        assert_eq!(reply_of(yes), Reply::Yes, "{yes:?}");
    }
    for no in ["n", "N\n", "no", " No \r\n"] {
        assert_eq!(reply_of(no), Reply::No, "{no:?}");
    }
    for unclear in ["maybe", "yep", "nope", "0", "y n", "q"] {
        assert_eq!(reply_of(unclear), Reply::Unclear, "{unclear:?}");
    }
}

fn asking(input: &str) -> (Asked, String) {
    let mut out = Vec::new();
    let asked = ask(
        &mut std::io::Cursor::new(input.as_bytes()),
        &mut out,
        "T\n",
        &mut || {},
    );
    (asked, String::from_utf8(out).unwrap())
}

#[test]
fn an_unclear_line_is_asked_about_once_more_and_end_of_input_is_no_answer() {
    let (asked, said) = asking("\n");
    assert_eq!(asked, Asked::Yes);
    assert_eq!(said, format!("T\n{OFFER_QUESTION}"));
    assert_eq!(asking("n\n").0, Asked::No);
    // End of input: nobody answered.
    assert_eq!(asking("").0, Asked::NoAnswer);
    // One more try, and then a real answer counts.
    let (asked, said) = asking("what\ny\n");
    assert_eq!(asked, Asked::Yes);
    assert_eq!(said.matches(OFFER_QUESTION).count(), 2, "{said}");
    assert!(said.contains("Please answer y or n"), "{said}");
    assert_eq!(asking("what\nno\n").0, Asked::No);
    // Twice unclear, or unclear and then the end: no answer, never a third
    // question.
    let (asked, said) = asking("what\nwhy\ny\n");
    assert_eq!(asked, Asked::NoAnswer);
    assert_eq!(said.matches(OFFER_QUESTION).count(), 2, "{said}");
    assert_eq!(asking("what\n").0, Asked::NoAnswer);
}

/// A terminal for the tests: what was typed and not yet read, which a flush
/// throws away as the real one does.
#[derive(Clone, Default)]
struct Typed(std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<u8>>>);

impl Typed {
    fn key(&self, text: &str) {
        self.0.borrow_mut().extend(text.bytes());
    }
}

impl std::io::Read for Typed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut queue = self.0.borrow_mut();
        let n = buf.len().min(queue.len());
        for slot in buf.iter_mut().take(n) {
            *slot = queue.pop_front().unwrap_or_default();
        }
        Ok(n)
    }
}

/// Keys pressed before the question was on screen are not its answer: the
/// pending input is thrown away immediately before each question is printed,
/// so an Enter typed ahead cannot install the mod.
#[test]
fn what_was_typed_before_the_question_is_not_its_answer() {
    // An Enter is waiting. Without the flush it would be a yes.
    let typed = Typed::default();
    typed.key("\n");
    let (pending, mut flushes) = (typed.clone(), 0);
    let mut out = Vec::new();
    let asked = ask(
        &mut std::io::BufReader::new(typed),
        &mut out,
        "T\n",
        &mut || {
            flushes += 1;
            pending.0.borrow_mut().clear();
        },
    );
    assert_eq!(
        asked,
        Asked::NoAnswer,
        "the typed-ahead Enter was thrown away"
    );
    assert_eq!(flushes, 1);

    // The flush comes before every question, the second one too: what is
    // typed after the first question and before the second is dropped, and
    // what is typed after the question was printed is the answer.
    let typed = Typed::default();
    typed.key("y\n");
    let (pending, later, mut flushes) = (typed.clone(), typed.clone(), 0);
    let mut out = Vec::new();
    let asked = ask(
        &mut std::io::BufReader::with_capacity(1, typed),
        &mut out,
        "T\n",
        &mut || {
            flushes += 1;
            pending.0.borrow_mut().clear();
            // What the person types once the question is on screen.
            later.key(if flushes == 1 { "what\ny\n" } else { "n\n" });
        },
    );
    assert_eq!(flushes, 2);
    assert_eq!(
        asked,
        Asked::No,
        "the y typed ahead of the second question is gone"
    );
}

/// The wording, in the order the plan gives: what it is, what you get, what
/// it changes, how to change your mind, and where the answer is kept, all
/// before the question.
#[test]
fn the_offer_says_what_it_is_what_it_gives_what_it_changes_and_how_to_undo_it() {
    let text = offer_text(Path::new("/cfg/atrium/mod"), false, "WHERE-KEPT");
    let order = [
        "a Claude Code plugin that atrium loads into the claude panes it hosts",
        "/cfg/atrium/mod",
        "status for certain",
        "without typing into its prompt and keeps its line breaks",
        "subagents become visible panes",
        "takes over Claude Code's Agent tool",
        "If it fails it lets Claude Code carry on",
        "a refusal that atrium itself returned still stands",
        "switches it off by itself if it stops answering or keeps failing",
        "`atrium mod install` says yes",
        "`atrium mod off` says no",
        "Nothing of the mod is written without a yes",
        "Your answer, yes or no, is kept in WHERE-KEPT",
    ];
    let mut at = 0;
    for piece in order {
        let found = text[at..]
            .find(piece)
            .unwrap_or_else(|| panic!("{piece:?} is missing or out of order in:\n{text}"));
        at += found + piece.len();
    }
    assert_eq!(text.lines().count(), 5, "{text}");
    // Nothing that was found untrue.
    for gone in ["never blocks", "if it breaks", "writes nothing"] {
        assert!(!text.contains(gone), "{gone:?} in: {text}");
    }
    assert!(!text.contains("older atrium"));
    // An unmarked mod is already there: a yes replaces its files, a no
    // turns the mod off, and both are said.
    let adopt = offer_text(Path::new("/cfg/atrium/mod"), true, "WHERE-KEPT");
    assert!(adopt.starts_with(&text));
    assert!(
        adopt.contains("older atrium")
            && adopt.contains("a yes replaces its files with this atrium's copy")
            && adopt.contains("a no leaves the files and turns the mod off"),
        "{adopt}"
    );
}

/// What an offer run did: the answer, what was said, and what was recorded.
fn offered(input: &str, dir: &Path, adopt: bool) -> (Asked, String, Vec<bool>) {
    let mut out = Vec::new();
    let mut recorded = Vec::new();
    let asked = run_offer(
        &mut std::io::Cursor::new(input.as_bytes()),
        &mut out,
        OfferRun {
            dir,
            adopt,
            root: None,
            kept: "THE-PLACE-TO-BE",
            flush: &mut || {},
            record: &mut |yes| {
                recorded.push(yes);
                Ok("THE-PLACE".to_string())
            },
        },
    );
    (asked, String::from_utf8(out).unwrap(), recorded)
}

#[test]
fn a_yes_installs_as_mod_install_does_and_is_recorded() {
    let dir = temp("yes");
    let (asked, said, recorded) = offered("\n", &dir, false);
    assert_eq!(asked, Asked::Yes);
    assert_eq!(folder_at(&dir), Folder::Ours, "the marker is there");
    assert!(status_at(&dir).current);
    assert_eq!(recorded, [true]);
    assert!(
        said.contains(&format!(
            "atrium: mod {} at {}: {} written, 0 unchanged",
            embedded_version(),
            dir.display(),
            FILES.len()
        )),
        "what it wrote is printed: {said}"
    );
    assert!(said.contains("kept in THE-PLACE"), "{said}");
    // Where the answer goes was said before the question was asked.
    assert!(
        said.find("is kept in THE-PLACE-TO-BE").unwrap() < said.find(OFFER_QUESTION).unwrap(),
        "{said}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_no_writes_nothing_and_is_recorded() {
    let dir = temp("no");
    let (asked, said, recorded) = offered("n\n", &dir, false);
    assert_eq!(asked, Asked::No);
    assert!(!dir.exists(), "nothing is written without a yes");
    assert_eq!(recorded, [false]);
    assert!(said.contains("no mod is written"), "{said}");
    assert!(said.contains("kept in THE-PLACE"), "{said}");
}

#[test]
fn no_answer_writes_nothing_and_records_nothing() {
    let dir = temp("eof");
    for input in ["", "huh\nwhat\n"] {
        let (asked, said, recorded) = offered(input, &dir, false);
        assert_eq!(asked, Asked::NoAnswer);
        assert!(!dir.exists());
        assert!(recorded.is_empty(), "asked again next time");
        assert!(said.contains("the next start asks again"), "{said}");
    }
}

#[test]
fn an_install_that_fails_records_nothing() {
    let base = temp("blocked");
    std::fs::create_dir_all(&base).unwrap();
    // A file where the folder should go.
    let dir = base.join("mod");
    std::fs::write(&dir, "in the way").unwrap();
    let (asked, said, recorded) = offered("y\n", &dir, false);
    assert_eq!(asked, Asked::Yes);
    assert!(
        recorded.is_empty(),
        "a yes that installed nothing is not kept"
    );
    assert!(said.contains("was not installed"), "{said}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_yes_over_an_unmarked_mod_marks_it_and_a_no_leaves_it_as_it_is() {
    let dir = temp("adopt");
    old_install(&dir, "0.2.0");
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
    let (_, said, recorded) = offered("no\n", &dir, true);
    assert_eq!(recorded, [false]);
    assert_eq!(folder_at(&dir), Folder::Unmarked);
    assert_eq!(
        std::fs::read_to_string(dir.join(MANIFEST)).unwrap(),
        manifest
    );
    assert!(said.contains("the mod is off in atrium's panes"), "{said}");

    let (_, _, recorded) = offered("yes\n", &dir, true);
    assert_eq!(recorded, [true]);
    assert_eq!(folder_at(&dir), Folder::Ours);
    assert!(status_at(&dir).current);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_record_is_said_and_the_install_stands() {
    let run = |input: &'static str, dir: &Path| {
        let mut out = Vec::new();
        run_offer(
            &mut std::io::Cursor::new(input.as_bytes()),
            &mut out,
            OfferRun {
                dir,
                adopt: false,
                root: None,
                kept: "X",
                flush: &mut || {},
                record: &mut |_| Err("read-only".to_string()),
            },
        );
        String::from_utf8(out).unwrap()
    };
    // A yes: the folder is installed and marked, so the offer is not made
    // again while it is there. The line says that, not "asks again".
    let dir = temp("unkept");
    let said = run("y\n", &dir);
    assert!(
        said.contains("the mod is installed; the answer could not be kept (read-only)")
            && said.contains("does not ask again while it is there"),
        "{said}"
    );
    assert!(!said.contains("the next start asks again"), "{said}");
    assert_eq!(folder_at(&dir), Folder::Ours);
    assert_eq!(
        offer_of(&OfferFacts {
            folder: Some(folder_at(&dir)),
            ..ASKED
        }),
        Offer::Not(NotAsked::Installed),
        "and that is what the next start does"
    );
    let _ = std::fs::remove_dir_all(&dir);
    // A no that could not be kept: nothing is there, so the next start asks.
    let dir = temp("unkept-no");
    let said = run("n\n", &dir);
    assert!(
        said.contains("could not be kept (read-only); the next start asks again"),
        "{said}"
    );
    assert!(!dir.exists());
}

#[test]
fn the_install_report_clears_the_breakers_record() {
    let (dir, root) = (temp("report"), temp("report-root"));
    write_record(&root, &TripRecord::now("2.1.325", "why", 1)).unwrap();
    let lines = install_report(&dir, Some(&root)).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].contains("record was cleared"), "{lines:?}");
    assert_eq!(read_record(&root), RecordRead::None);
    assert_eq!(install_report(&dir, Some(&root)).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn claude_is_looked_for_under_every_name_it_goes_by() {
    let dir = temp("path");
    std::fs::create_dir_all(&dir).unwrap();
    let names = |list: &[&str]| -> Vec<String> { list.iter().map(|n| n.to_string()).collect() };
    let exts = vec![".CMD".to_string(), String::new()];
    let dirs = vec![dir.join("nowhere"), dir.clone()];
    assert!(!claude_on_path(&names(&["claude"]), &dirs, &exts));
    std::fs::write(dir.join("claude2.CMD"), "").unwrap();
    assert!(!claude_on_path(&names(&["claude"]), &dirs, &exts));
    assert!(claude_on_path(&names(&["claude", "claude2"]), &dirs, &exts));
    std::fs::write(dir.join("claude"), "").unwrap();
    assert!(claude_on_path(&names(&["claude"]), &dirs, &exts));
    // An alias that is a path counts when the file is there, not otherwise.
    let full = dir.join("claude").to_string_lossy().into_owned();
    assert!(claude_on_path(&names(&[&full]), &[], &exts));
    let gone = dir
        .join("gone")
        .join("claude")
        .to_string_lossy()
        .into_owned();
    assert!(!claude_on_path(&names(&[&gone]), &[], &exts));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- what a yes covers at launch ----

fn launch(dir: &Path, yes: bool, env_named: bool) -> (Vec<String>, Launched) {
    launch_in(Some(dir), None, || None, 0, yes, env_named)
}

#[test]
fn after_a_yes_a_missing_folder_is_written_again_with_one_line() {
    let dir = temp("missing");
    // Without a recorded yes nothing is created, as before.
    let (lines, state) = launch(&dir, false, false);
    assert!(lines.is_empty(), "{lines:?}");
    assert!(!dir.exists() && !state.current);

    let (lines, state) = launch(&dir, true, false);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("written to")
            && lines[0].contains("it was missing")
            && lines[0].contains("you said yes")
            && lines[0].ends_with("`atrium mod off` stops this"),
        "{lines:?}"
    );
    assert_eq!(folder_at(&dir), Folder::Ours);
    assert!(state.current && state.loadable);
    // The next launch has nothing to do and nothing to say.
    let (lines, _) = launch(&dir, true, false);
    assert!(lines.is_empty(), "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn after_a_yes_an_older_marked_folder_is_refreshed_and_a_newer_one_never_downgraded() {
    let dir = temp("older");
    install_at(&dir).unwrap();
    let hooks = dir.join("hooks/status.ts");
    std::fs::write(&hooks, "// an older atrium's").unwrap();
    std::fs::write(
        dir.join(MARKER),
        "{\"mod\":\"0.0.1\",\"hash\":\"0000000000000000\"}\n",
    )
    .unwrap();
    let (lines, state) = launch(&dir, true, false);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("refreshed from this atrium"), "{lines:?}");
    assert!(state.current);
    assert_eq!(
        marker_at(&dir).unwrap(),
        (embedded_version(), embedded_hash())
    );

    // A marker from a newer atrium: left exactly as it is, yes or not.
    std::fs::write(&hooks, "// a newer atrium's").unwrap();
    let newer = "{\"mod\":\"999.0.0\",\"hash\":\"ffffffffffffffff\"}\n";
    std::fs::write(dir.join(MARKER), newer).unwrap();
    let (lines, state) = launch(&dir, true, false);
    assert!(lines[0].contains("newer than"), "{lines:?}");
    assert!(!state.current && state.loadable);
    assert_eq!(
        std::fs::read_to_string(&hooks).unwrap(),
        "// a newer atrium's"
    );
    assert_eq!(std::fs::read_to_string(dir.join(MARKER)).unwrap(), newer);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn after_a_yes_an_unmarked_folder_becomes_atriums_own_unless_it_is_newer() {
    let dir = temp("unmarked");
    old_install(&dir, "0.2.0");
    // No yes: loaded as it is, never written to.
    let (lines, _) = launch(&dir, false, false);
    assert!(lines[0].contains("no .atrium-mod marker"), "{lines:?}");
    assert_eq!(folder_at(&dir), Folder::Unmarked);

    let (lines, state) = launch(&dir, true, false);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("from an older atrium"), "{lines:?}");
    assert_eq!(folder_at(&dir), Folder::Ours);
    assert!(state.current);

    // An unmarked mod whose manifest names a later version is not replaced.
    let later = temp("unmarked-later");
    old_install(&later, "999.0.0");
    assert_eq!(refresh_after_yes(&later, false), None);
    let (_, state) = launch(&later, true, false);
    assert_eq!(folder_at(&later), Folder::Unmarked);
    assert!(!state.current);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&later);
}

#[test]
fn a_folder_named_by_the_environment_or_inside_a_checkout_is_never_touched() {
    // Named by ATRIUM_MOD: nothing is created, nothing marked.
    let named = temp("env");
    let (lines, _) = launch(&named, true, true);
    assert!(lines.is_empty() && !named.exists(), "{lines:?}");
    old_install(&named, "0.2.0");
    let manifest = std::fs::read_to_string(named.join(MANIFEST)).unwrap();
    let (lines, _) = launch(&named, true, true);
    assert!(lines[0].contains("loaded as it is"), "{lines:?}");
    assert_eq!(folder_at(&named), Folder::Unmarked);
    assert_eq!(
        std::fs::read_to_string(named.join(MANIFEST)).unwrap(),
        manifest
    );

    // Inside a git checkout: the same, with no variable naming it.
    let repo = temp("checkout");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let inside = repo.join("mod");
    assert!(developer_folder(&inside, false));
    let (lines, _) = launch(&inside, true, false);
    assert!(lines.is_empty() && !inside.exists(), "{lines:?}");
    old_install(&inside, "0.2.0");
    launch(&inside, true, false);
    assert_eq!(folder_at(&inside), Folder::Unmarked);
    assert_eq!(refresh_after_yes(&inside, true), None);
    let _ = std::fs::remove_dir_all(&named);
    let _ = std::fs::remove_dir_all(&repo);
}

/// A breaker record for this same Claude Code and this same mod, and a user
/// who deleted the folder: the kept yes writes it back, and it is the very
/// mod the record names. Nothing changed, so the mod stays off and the line
/// says why. Only a folder that held another mod (unmarked, or older) is a
/// change.
#[test]
fn a_folder_written_back_after_a_yes_is_no_change_for_the_breaker() {
    let (dir, root) = (temp("same"), temp("same-root"));
    write_record(&root, &TripRecord::now("2.1.325", "why", 1)).unwrap();
    let claude = || Some("2.1.325".to_string());
    let (lines, state) = launch_in(Some(&dir), Some(&root), claude, 60_000, true, false);
    assert_eq!(folder_at(&dir), Folder::Ours, "the folder is rewritten");
    assert!(matches!(state.launch, Launch::Tripped(_)), "{state:?}");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("it was missing"), "{lines:?}");
    assert!(
        lines[1].contains("are unchanged, so it stays off") && lines[1].contains("(why)"),
        "{lines:?}"
    );
    assert!(!lines.join(" ").contains("tried again"), "{lines:?}");
    assert!(!refresh_after_yes(&temp("same-flag"), false).unwrap().1);
    let _ = std::fs::remove_dir_all(temp("same-flag"));

    // An unmarked mod from an older atrium was what ran: replacing it is a
    // change, and the mod is tried again.
    let old = temp("same-old");
    old_install(&old, "0.2.0");
    let (lines, state) = launch_in(Some(&old), Some(&root), claude, 60_000, true, false);
    assert!(matches!(state.launch, Launch::Retrying(_)), "{state:?}");
    assert!(lines[1].contains("tried again"), "{lines:?}");
    // And so is a marked folder the heal had to rewrite.
    std::fs::write(old.join("hooks/status.ts"), "// edited").unwrap();
    let (_, state) = launch_in(Some(&old), Some(&root), claude, 60_000, true, false);
    assert!(matches!(state.launch, Launch::Retrying(_)), "{state:?}");
    for d in [dir, root, old] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---- nobody there ----

#[test]
fn the_absent_line_is_for_no_mod_and_no_answer_and_is_silent_after_a_no() {
    let absent = Some(Folder::Absent);
    let line = absent_note(true, Said::Unset, absent, "/cfg/mod").unwrap();
    assert_eq!(line, not_installed_line("/cfg/mod"));
    assert!(
        line.contains("not installed at /cfg/mod")
            && line.contains("status by inference")
            && line.contains("`atrium mod install` adds it"),
        "{line}"
    );
    assert_eq!(line.lines().count(), 1);
    // A recorded no is `"mod": false`: the session does not use the mod.
    assert_eq!(absent_note(false, Said::No, absent, "/d"), None);
    assert_eq!(absent_note(true, Said::No, absent, "/d"), None);
    // A fleet's own `"mod": false`.
    assert_eq!(absent_note(false, Said::Unset, absent, "/d"), None);
    // A recorded yes: the launch writes the folder, there is nothing to add.
    assert_eq!(absent_note(true, Said::Yes, absent, "/d"), None);
    // A mod is there, marked or not.
    for there in [Some(Folder::Ours), Some(Folder::Unmarked), None] {
        assert_eq!(absent_note(true, Said::Unset, there, "/d"), None);
    }
    // A folder the config names with nothing in it is still worth the line.
    assert!(absent_note(true, Said::Managed, absent, "/d").is_some());
}

#[test]
fn describe_says_a_missing_folder_is_written_at_launch_after_a_yes() {
    let none = Status {
        dir: Some(PathBuf::from("/d")),
        installed: None,
        current: false,
    };
    let (line, warn) = describe(&none, true, true);
    assert!(
        line.contains("written there when the first claude pane starts")
            && line.contains("you said yes"),
        "{line}"
    );
    assert!(!warn);
    let (line, warn) = describe(&none, true, false);
    assert_eq!(line, not_installed_line("/d"));
    assert!(warn);
    // Off is off, whatever was said.
    assert!(describe(&none, false, true).0.contains("off"));
}

// ---- which path a named teammate takes ----

const FACTS: TeammateFacts = TeammateFacts {
    ctl: true,
    mod_expect: Expect::On,
    subagents: Subagents::Panes,
    shim_on: true,
    teams_var: false,
};

#[test]
fn with_the_mod_on_a_named_teammate_is_a_pane_through_the_mod() {
    let line = teammates_line(&FACTS);
    assert_eq!(
        line,
        "teams: named subagents and teammates become atrium panes through the mod \
         (subagents: panes)"
    );
    // The shim is not the path, so the teams variable is not this line's.
    assert!(!line.contains("tmux") && !line.contains(ENV_AGENT_TEAMS));
    assert_eq!(
        teammates_line(&TeammateFacts {
            shim_on: false,
            teams_var: true,
            ..FACTS
        }),
        line
    );
}

#[test]
fn with_the_mod_off_and_the_shim_on_teammates_land_through_the_shim() {
    let off = TeammateFacts {
        mod_expect: Expect::Off,
        ..FACTS
    };
    assert_eq!(
        teammates_line(&off),
        "teams: the mod is off; Claude Code's own teammates land as panes through the tmux \
         shim; teams also need CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 in the environment or in \
         Claude Code's settings (atrium cannot see it in its environment)"
    );
    // Seen in atrium's environment: the second clause goes.
    assert_eq!(
        teammates_line(&TeammateFacts {
            teams_var: true,
            ..off
        }),
        "teams: the mod is off; Claude Code's own teammates land as panes through the tmux shim"
    );
    // It never says the variable "is off": atrium cannot know that.
    assert!(!teammates_line(&off).contains("is not set"));
}

#[test]
fn with_neither_subagents_run_inside_their_agent() {
    let neither = TeammateFacts {
        mod_expect: Expect::Off,
        shim_on: false,
        ..FACTS
    };
    assert_eq!(
        teammates_line(&neither),
        "teams: the mod is off and the tmux shim is off: subagents and teammates run inside \
         their agent and are not visible to atrium"
    );
    // No control plane: neither the mod nor the shim reaches atrium.
    let line = teammates_line(&TeammateFacts {
        ctl: false,
        ..FACTS
    });
    assert!(
        line.starts_with("teams: no control plane:") && line.contains("not visible to atrium"),
        "{line}"
    );
}

#[test]
fn the_subagents_policy_in_force_is_part_of_the_line() {
    let native = TeammateFacts {
        subagents: Subagents::Native,
        ..FACTS
    };
    let line = teammates_line(&native);
    assert!(
        line.contains("(subagents: native)") && line.contains("through the tmux shim"),
        "{line}"
    );
    let line = teammates_line(&TeammateFacts {
        shim_on: false,
        ..native
    });
    assert!(
        line.contains("(subagents: native)") && line.contains("not visible to atrium"),
        "{line}"
    );
    // `deny` holds with or without the mod: the launch flags carry it.
    for mod_expect in [Expect::On, Expect::Likely, Expect::Off] {
        let line = teammates_line(&TeammateFacts {
            subagents: Subagents::Deny,
            mod_expect,
            ..FACTS
        });
        assert!(line.contains("(subagents: deny)"), "{line}");
    }
    for facts in [FACTS, native] {
        assert_eq!(teammates_line(&facts).lines().count(), 1);
    }
}

#[test]
fn the_teams_variable_counts_only_when_it_is_switched_on() {
    for on in ["1", "true", " yes "] {
        assert!(teams_var_on(Some(on)), "{on:?}");
    }
    for off in ["", " ", "0", "false", "FALSE", "no", "off"] {
        assert!(!teams_var_on(Some(off)), "{off:?}");
    }
    assert!(!teams_var_on(None));
}

/// Not settled before a pane starts: the line says "expected".
#[test]
fn a_mod_that_is_only_likely_is_said_to_be_expected() {
    let line = teammates_line(&TeammateFacts {
        mod_expect: Expect::Likely,
        ..FACTS
    });
    assert_eq!(
        line,
        "teams: named subagents and teammates are expected to become atrium panes through the \
         mod (subagents: panes); the first claude pane settles whether the mod loads"
    );
}

const INSTALLED: ExpectFacts = ExpectFacts {
    enabled: true,
    installed: true,
    will_write: false,
    mixed: false,
    differs: false,
};

/// The teams line and the session agree, because both decide with
/// `launch_of`: a record for this Claude Code and mod keeps the mod off; a
/// Claude Code that was upgraded since means it is tried again.
#[test]
fn the_mod_is_expected_as_the_launch_will_decide_it() {
    let record = || RecordRead::Tripped(TripRecord::now("2.1.325", "why", 1));
    let claude = |v: &'static str| move || Some(v.to_string());
    assert_eq!(expect_of(&INSTALLED, RecordRead::None, || None), Expect::On);
    assert_eq!(
        expect_of(&INSTALLED, RecordRead::Unreadable, || None),
        Expect::On
    );
    assert_eq!(
        expect_of(&INSTALLED, record(), claude("2.1.325")),
        Expect::Off,
        "the same claude and the same mod: it stays off"
    );
    assert_eq!(
        expect_of(&INSTALLED, record(), claude("2.2.0")),
        Expect::Likely,
        "claude was upgraded: the launch tries the mod again"
    );
    assert_eq!(
        expect_of(&INSTALLED, record(), || None),
        Expect::Off,
        "an unknown version is not a changed one"
    );
    // Files the launch is about to replace: the mod on disk will not be the
    // one that tripped.
    let differs = ExpectFacts {
        differs: true,
        ..INSTALLED
    };
    assert_eq!(
        expect_of(&differs, record(), claude("2.1.325")),
        Expect::Likely
    );
    assert_eq!(
        expect_of(&differs, RecordRead::None, || None),
        Expect::Likely
    );
    // A folder the kept yes will write back is this binary's mod again: no
    // change for the breaker, and only likely until it is written.
    let will_write = ExpectFacts {
        installed: false,
        will_write: true,
        ..INSTALLED
    };
    assert_eq!(
        expect_of(&will_write, RecordRead::None, || None),
        Expect::Likely
    );
    assert_eq!(
        expect_of(&will_write, record(), claude("2.1.325")),
        Expect::Off
    );
    // A folder left a mix of versions may not load.
    let mixed = ExpectFacts {
        mixed: true,
        ..INSTALLED
    };
    assert_eq!(expect_of(&mixed, RecordRead::None, || None), Expect::Likely);
    // Not used, or nothing there and nothing to come: off, and claude is
    // never asked.
    let never = || -> Option<String> { panic!("claude is not asked") };
    for off in [
        ExpectFacts {
            enabled: false,
            ..INSTALLED
        },
        ExpectFacts {
            installed: false,
            ..INSTALLED
        },
    ] {
        assert_eq!(expect_of(&off, record(), never), Expect::Off);
    }
    assert_eq!(expect_of(&INSTALLED, RecordRead::None, never), Expect::On);
}

/// A platform config directory inside a larger repository (dotfiles over
/// `~/.config` or `~`) does not make the mod folder a checkout of the mod.
#[test]
fn a_config_directory_inside_a_dotfiles_repo_is_not_a_checkout_of_the_mod() {
    let home = temp("dotfiles");
    let platform = home.join(".config").join("atrium");
    let folder = platform.join("mod");
    std::fs::create_dir_all(&folder).unwrap();
    for git in [home.join(".git"), home.join(".config").join(".git")] {
        std::fs::create_dir_all(&git).unwrap();
        assert!(
            !developer_folder_in(&folder, false, Some(&platform)),
            "{} is above the platform directory",
            git.display()
        );
    }
    // The platform directory itself under git is the same case.
    std::fs::create_dir_all(platform.join(".git")).unwrap();
    assert!(!developer_folder_in(&folder, false, Some(&platform)));
    // The mod folder itself as a checkout is one.
    std::fs::create_dir_all(folder.join(".git")).unwrap();
    assert!(developer_folder_in(&folder, false, Some(&platform)));
    std::fs::remove_dir_all(folder.join(".git")).unwrap();
    // A folder outside the platform directory is looked at all the way up.
    let elsewhere = home.join("src").join("mine");
    std::fs::create_dir_all(&elsewhere).unwrap();
    assert!(developer_folder_in(&elsewhere, false, Some(&platform)));
    assert!(developer_folder_in(&elsewhere, false, None));
    // Named by ATRIUM_MOD: always.
    assert!(developer_folder_in(&folder, true, Some(&platform)));
    // atrium's own source `mod/`, with no `.git` in sight: known by its shape.
    let tree = temp("source-tree");
    let source = tree.join("mod");
    for d in ["hooks", "types", "tests"] {
        std::fs::create_dir_all(source.join(d)).unwrap();
    }
    assert!(
        !developer_folder_in(&source, false, None),
        "no Cargo.toml beside it"
    );
    std::fs::write(tree.join("Cargo.toml"), "").unwrap();
    assert!(developer_folder_in(&source, false, None));
    // An installed folder never has that shape, and a marker rules it out.
    std::fs::write(source.join(MARKER), "{}").unwrap();
    assert!(!developer_folder_in(&source, false, None));
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&tree);
}
