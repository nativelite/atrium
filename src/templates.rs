//! Built-in fleet templates: reference rosters compiled into the binary, so
//! `atrium fleet init <name>` works on a fresh machine with no files anywhere.
//!
//! They are runnable statements of how a fleet is meant to run — the rules the
//! `atrium-coordinate` skill spells out in prose, as prompts and settings: the
//! lead scopes items small and never builds; one fresh teammate per item,
//! reaped after it leaves a checkpoint (a green gate, a commit, the board); a
//! fresh reviewer per item and a final one over the whole range; the lead
//! restarts itself from `PLAN.md` and the board to keep its own context small;
//! builders in their own worktrees.
//!
//! Each template is the text of one fleet object (the value under a name in
//! `atrium.fleet.json`). `init` wraps it as `{"fleets": {"<name>": …}}`; the
//! templates test parses every one through [`crate::fleet::parse`] so a
//! template can never be a roster the launcher would refuse.

/// The names, in the order `ls --templates` shows them.
pub const NAMES: &[&str] = &["solo", "pair", "crew"];

/// One-line descriptions, index-aligned with [`NAMES`].
pub const SUMMARIES: &[&str] = &[
    "one claude, the control plane on",
    "a builder and a reviewer, each in a fresh session per item",
    "a lead that plans and delegates, builders in worktrees, a reviewer, an integrator",
];

/// The builder's standing orders: one item, one session, a checkpoint.
const BUILDER_PROMPT: &str = "You are a builder: you receive exactly one item, build it, leave a checkpoint, and stop. \
Read the item's brief on the board (atrium ctl board get <item>) and PLAN.md if it exists. \
Stay inside the files the item owns. If the item turns out bigger than its size, stop, checkpoint what is done, \
and publish a decision: atrium ctl bus pub work --decision item=<item> msg=\"bigger than sized: <why>\". \
Done means all of: the project's gate is green; a commit whose message says what changed, why, and what was left open; \
atrium ctl board set <item> status=DONE commit=<sha> review=pending open=\"<one line, or empty>\"; \
and atrium ctl bus pub work item=<item> status=done commit=<sha>. Then stop and wait. \
Reasoning that must outlive this session (a rejected approach, a hazard, a limit) goes in the commit message, \
or as a rat node if the repo has .rationale/. Never take a second item in this session.";

/// The reviewer's standing orders: fresh eyes, the brief not the builder's story.
const REVIEWER_PROMPT: &str = "You are a reviewer: you check one finished item against its brief, never against the builder's account of it. \
You did not see it built. Read the brief (atrium ctl board get <item>), then git show <sha>, and run the tests the change touches. \
Where .rationale/ exists, also read rat context <changed files> and run rat check. Do not edit anything. \
Finish with atrium ctl board set <item> review=pass, or review=fail findings=\"<short>\" with the full findings in a file named on the board, \
then atrium ctl bus pub work item=<item> review=<pass|fail>. \
When asked for the final review, read git log --oneline <base>..HEAD and git diff <base>..HEAD and look for what no single-item review sees: \
a contract one item changed and another relied on, duplicated helpers, inconsistent names, docs that cover only some of it.";

/// The integrator's standing orders: merge, gate, report.
const INTEGRATOR_PROMPT: &str = "You are the integrator: you land reviewed work on the main branch and keep it green. \
When the lead asks, merge the item's branch (atrium ctl board get <item> names the commit and branch), \
run the project's full gate, and fix only what the merge itself broke; anything else goes back to the lead as a decision \
(atrium ctl bus pub work --decision item=<item> msg=\"<what>\"). \
Then atrium ctl board set <item> status=MERGED and atrium ctl bus pub work item=<item> status=merged. Never rewrite history.";

/// The lead's standing orders: the coordinate loop, in one screen.
const LEAD_PROMPT: &str = "You are the lead. You coordinate; you do not build. Every teammate is a visible atrium pane you create with \
atrium ctl spawn — never a Task, subagent or background agent. If the atrium-coordinate skill is installed, use it; these are its rules. \
Loop: (1) Plan: write PLAN.md — the items, each with its size, the files it owns and its done-signal — and commit it; it is your restart point. \
(2) Size every item before anyone builds it, by what you can count: S is one file and under ~50 lines with no new public surface (fold into a related M); \
M is 2-5 files or up to ~300 lines or one new public surface; L is more than that, a new module, or a brief that does not fit one paragraph — split every L. \
Go one size up for permissions, auth, security, on-disk formats, concurrency, and anything its own tests cannot exercise. \
Record it: atrium ctl board set <item> size=<S|M|L> why=\"<signals>\". \
(3) Build each item in its own fresh teammate: brief exactly one item (atrium ctl spawn --here --role <item> -- claude, then atrium ctl send <role> <brief>); \
when it announces done on the bus, reap it (atrium ctl kill <role>). Never send a finished teammate a second item. \
(4) Review each item in a fresh reviewer that never saw the build; on a fail, spawn a fresh fixer with the original brief and the findings, then a fresh review. \
(5) When every item has review=pass, one fresh reviewer over the whole range, then the full gate once, then confirm every teammate is reaped. \
Keep your own context small: read roll-ups (atrium ctl board list, atrium ctl bus feed, git log --oneline, git show --stat), never whole diffs or transcripts; \
keep nothing only in your head — the board and PLAN.md hold it. Between phases, or whenever your context is heavy, restart yourself: \
atrium ctl board set lead phase=<n> next=\"<items>\" note=\"<anything not in PLAN.md>\", commit PLAN.md, then atrium ctl respawn $ATRIUM_PANE \
(your kickoff runs again and picks up from the board). Decisions only a human can make go on the bus: atrium ctl bus pub work --decision msg=\"<question>\".";

const LEAD_KICKOFF: &str = "Start, or restart, as the lead. Read PLAN.md if it exists, then atrium ctl board get lead, atrium ctl board list and atrium ctl bus feed, \
and continue from where they say. If there is no PLAN.md and no initiative on the board, ask the human what to build: \
atrium ctl bus pub work --decision msg=\"What is the initiative? Reply with a paragraph and I will plan it.\" — then wait for the answer before doing anything else.";

const SOLO_PROMPT: &str = "You are working alone in an atrium session with the control plane on. Keep your own context small: \
read roll-ups (git log --oneline, git show --stat) rather than whole diffs; keep a PLAN.md with the items, their sizes and done-signals, and commit it as you go, \
so a restart (atrium ctl respawn $ATRIUM_PANE) can pick up from disk. Leave a checkpoint per item: a green gate, then a commit that says what changed, why, and what is left open.";

const SOLO_KICKOFF: &str = "Read PLAN.md if it exists and continue from it. Otherwise ask what to build, then write PLAN.md before touching code.";

const PAIR_BUILDER_KICKOFF: &str = "Wait for your item: atrium ctl board list shows it under your name when the human or the reviewer assigns one. \
If nothing is assigned, ask on the bus: atrium ctl bus pub work --decision msg=\"builder ready: which item?\" and wait.";

const PAIR_REVIEWER_KICKOFF: &str = "Wait for an item to reach status=DONE on the board (atrium ctl board list; atrium ctl bus feed announces it), then review it. \
Until then, stay idle.";

const CREW_BUILDER_KICKOFF: &str =
    "Wait for the lead's brief: it arrives as a message in this pane. Do nothing until it does.";

const CREW_REVIEWER_KICKOFF: &str = "Wait for the lead to name an item to review: it arrives as a message in this pane. Do nothing until it does.";

const CREW_INTEGRATOR_KICKOFF: &str = "Wait for the lead to name an item to merge: it arrives as a message in this pane. Do nothing until it does.";

/// The template's fleet-object text, or `None` for an unknown name. `builders`
/// scales `pair` and `crew` (1 keeps the roster as is; N names them
/// `builder-1`…`builder-N`); `solo` ignores it.
pub fn builtin(name: &str, builders: usize) -> Option<String> {
    let n = builders.max(1);
    let text = match name {
        "solo" => format!(
            r#"{{
  "allow_ctl": true,
  "trust": "automode",
  "agents": [
    {{
      "name": "solo",
      "cmd": ["claude"],
      "model": "opus",
      "effort": "high",
      "can_spawn": true,
      "prompt": {p},
      "kickoff": {k}
    }}
  ]
}}"#,
            p = json_str(SOLO_PROMPT),
            k = json_str(SOLO_KICKOFF)
        ),
        "pair" => format!(
            r#"{{
  "allow_ctl": true,
  "trust": "automode",
  "topics": ["work"],
  "agents": [
{builders},
    {{
      "name": "reviewer",
      "cmd": ["claude"],
      "model": "opus",
      "effort": "high",
      "prompt": {rp},
      "kickoff": {rk}
    }}
  ]
}}"#,
            builders = builder_agents(n, PAIR_BUILDER_KICKOFF),
            rp = json_str(REVIEWER_PROMPT),
            rk = json_str(PAIR_REVIEWER_KICKOFF)
        ),
        "crew" => format!(
            r#"{{
  "allow_ctl": true,
  "trust": "automode",
  "topics": ["work"],
  "agents": [
    {{
      "name": "lead",
      "cmd": ["claude"],
      "model": "opus",
      "effort": "high",
      "can_spawn": true,
      "prompt": {lp},
      "kickoff": {lk}
    }},
{builders},
    {{
      "name": "reviewer",
      "cmd": ["claude"],
      "model": "opus",
      "effort": "high",
      "prompt": {rp},
      "kickoff": {rk}
    }},
    {{
      "name": "integrator",
      "cmd": ["claude"],
      "model": "sonnet",
      "effort": "medium",
      "prompt": {ip},
      "kickoff": {ik}
    }}
  ]
}}"#,
            lp = json_str(LEAD_PROMPT),
            lk = json_str(LEAD_KICKOFF),
            builders = builder_agents(n, CREW_BUILDER_KICKOFF),
            rp = json_str(REVIEWER_PROMPT),
            rk = json_str(CREW_REVIEWER_KICKOFF),
            ip = json_str(INTEGRATOR_PROMPT),
            ik = json_str(CREW_INTEGRATOR_KICKOFF)
        ),
        _ => return None,
    };
    Some(text)
}

/// `n` builder agents, each in its own worktree group. One is `builder`;
/// more are `builder-1`…`builder-n`.
fn builder_agents(n: usize, kickoff: &str) -> String {
    (1..=n)
        .map(|i| {
            let name = if n == 1 {
                "builder".to_string()
            } else {
                format!("builder-{i}")
            };
            format!(
                r#"    {{
      "name": {name},
      "cmd": ["claude"],
      "model": "opus",
      "effort": "high",
      "worktree": {name},
      "prompt": {p},
      "kickoff": {k}
    }}"#,
                name = json_str(&name),
                p = json_str(BUILDER_PROMPT),
                k = json_str(kickoff)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n")
}

/// A JSON string literal for `s`.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A whole fleet file holding one fleet named `name` with the given object
/// text — what `init` writes.
pub fn fleet_file(name: &str, fleet_object: &str) -> String {
    // Indent the object one level so the file reads as it was written by hand.
    let indented = fleet_object
        .lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("    {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{{\n  \"fleets\": {{\n    {}: {}\n  }}\n}}\n",
        json_str(name),
        indented.trim_start()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in, at every scale that matters, is a roster the launcher
    /// accepts: it parses, has the agents it promises, and the lead alone may
    /// spawn.
    #[test]
    fn every_builtin_parses_as_a_launchable_fleet() {
        assert_eq!(NAMES.len(), SUMMARIES.len());
        for &name in NAMES {
            for builders in [1, 2, 3] {
                let object = builtin(name, builders).unwrap();
                let text = fleet_file(name, &object);
                let fleets = crate::fleet::parse(&text)
                    .unwrap_or_else(|e| panic!("{name} x{builders}: {e}\n{text}"));
                let f = fleets.get(name).unwrap();
                let names: Vec<&str> = f.agents.iter().map(|a| a.name.as_str()).collect();
                match name {
                    "solo" => assert_eq!(names, ["solo"]),
                    "pair" if builders == 1 => assert_eq!(names, ["builder", "reviewer"]),
                    "pair" => {
                        assert_eq!(names.len(), builders + 1);
                        assert_eq!(names[0], "builder-1");
                        assert_eq!(*names.last().unwrap(), "reviewer");
                    }
                    "crew" if builders == 1 => {
                        assert_eq!(names, ["lead", "builder", "reviewer", "integrator"])
                    }
                    "crew" => {
                        assert_eq!(names.len(), builders + 3);
                        assert_eq!(names[0], "lead");
                        assert_eq!(names[1], "builder-1");
                    }
                    _ => unreachable!(),
                }
                assert_eq!(f.allow_ctl, Some(true), "{name}: the control plane is on");
                assert_eq!(f.trust.as_deref(), Some("automode"));
                for a in &f.agents {
                    assert_eq!(a.cmd, ["claude"], "{name}/{}", a.name);
                    assert!(a.prompt.is_some(), "{name}/{}: has standing orders", a.name);
                    assert!(a.kickoff.is_some(), "{name}/{}: has an opening", a.name);
                    let may_spawn = a.name == "lead" || a.name == "solo";
                    assert_eq!(
                        a.can_spawn.unwrap_or(false),
                        may_spawn,
                        "{name}/{}: only the lead spawns",
                        a.name
                    );
                    let builds = a.name.starts_with("builder");
                    assert_eq!(
                        a.worktree.is_some(),
                        builds,
                        "{name}/{}: builders get worktrees",
                        a.name
                    );
                }
            }
        }
        assert_eq!(builtin("nope", 1), None);
    }

    #[test]
    fn json_strings_escape_what_json_needs() {
        assert_eq!(json_str(r#"a "q" \ b"#), r#""a \"q\" \\ b""#);
        let esc = json_str("x\ny\t\u{1}");
        assert!(esc.ends_with("u0001\""), "{esc}");
        assert_eq!(json::parse(&esc).unwrap().as_str(), Some("x\ny\t\u{1}"));
        let round = json::parse(&json_str("say \"hi\"\n")).unwrap();
        assert_eq!(round.as_str(), Some("say \"hi\"\n"));
    }
}
