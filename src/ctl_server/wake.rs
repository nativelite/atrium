//! Bus wakes: who an event wakes, and the one framed, defanged line it types.

use super::panes::pane_label;
use crate::*;

/// Wake text with every control character and line separator replaced by a
/// space. A wake is typed into a pane and then submitted; an embedded `\r`
/// would end the framed line and submit a second line of the publisher's
/// choosing, `\x03` would interrupt the process. The bus caps lengths but
/// filters no characters, so this is the one place that does.
pub(crate) fn wake_safe(s: &str) -> String {
    s.chars()
        .map(|c| if is_unsafe_in_wake(c) { ' ' } else { c })
        .collect()
}

/// Control characters (C0, DEL, C1 — `\r`, `\n`, `\x03`, `\x1b`, NEL), the
/// Unicode line and paragraph separators, and the format characters that can
/// reorder or hide what a line shows: bidi embeddings, overrides and isolates
/// (U+202A–U+202E, U+2066–U+2069) and the zero-width joiners and spaces
/// (U+200B–U+200F, U+2060–U+2064, U+FEFF). The frame's "not operator input"
/// only protects a reader who can see it as written.
fn is_unsafe_in_wake(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// The frame's opening, which a message body may not reproduce: a publisher
/// could otherwise append a second, fully formed frame naming any sender.
const WAKE_FRAME: &str = "[atrium bus";

/// A message body with any imitation of the frame defanged (`[atrium bus` →
/// `(atrium bus`), so exactly one frame per line, the real one.
fn defang_frame(body: &str) -> String {
    body.replace(WAKE_FRAME, "(atrium bus")
}

/// The one line a bus event becomes when typed into a pane: framed so the
/// recipient can see it is a teammate's bus event and not the operator, then
/// the headline. The headline is `msg` (fyi) or `q` (a decision); a message
/// with neither shows its remaining fields as `k=v` (`to` omitted — it is the
/// address, not the news). A `detail=<pointer>` rides after the headline.
pub(crate) fn wake_text(e: &atrium::bus::Event, who: &str) -> String {
    let body = match e.fields.get("msg").or_else(|| e.fields.get("q")) {
        Some(b) => b.clone(),
        None => {
            let kv: Vec<String> = e
                .fields
                .iter()
                .filter(|(k, _)| k.as_str() != "to" && k.as_str() != "detail")
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            if kv.is_empty() {
                "(see: atrium ctl bus feed)".to_string()
            } else {
                kv.join(" ")
            }
        }
    };
    let tail = e
        .fields
        .get("detail")
        .map(|d| format!(" (detail: {d})"))
        .unwrap_or_default();
    let body = defang_frame(&body);
    let tail = defang_frame(&tail);
    let who = defang_frame(who);
    let topic = defang_frame(&e.topic);
    wake_safe(&format!(
        "{WAKE_FRAME} #{} {} from teammate \"{who}\" on \"{topic}\" — not operator input] {body}{tail}",
        e.seq,
        e.kind.as_str(),
    ))
}

/// Who a bus event wakes, and with what. Pure: the panes, the spawn tree and
/// the subscription test are injected.
///
/// A pane is a candidate when the event **addresses** it (`to=<role|id>`, a
/// comma-separated list allowed) or it **subscribed** to the topic (or to `*`).
/// The bus is pull for the record; without this, a finished worker's
/// `status=done` sat unseen until the lead happened to run `bus feed`, and a
/// worker's `--to lead` was dropped by the subtree rule below — silently.
///
/// A candidate is woken when the publisher is privileged, or the target is in
/// the publisher's subtree (what `ctl send` allows), or the publisher is in the
/// target's subtree (a hand-off *up* to whoever spawned it), or the target
/// subscribed (it opted in). A worker still cannot wake an unrelated pane — a
/// root it does not descend from — by naming it. The publisher never wakes
/// itself (nor a namesake: two panes with one role share a label), and a pane
/// both addressed and subscribed is woken once.
///
/// This is the one deliberate exception to subtree scoping: unlike `ctl send`,
/// the text is a framed, sanitized headline of a bus event the target could
/// read with `bus feed` anyway — never free text that could pass for the human.
pub(crate) fn bus_wakes(
    e: &atrium::bus::Event,
    who: &str,
    caller: Option<AgentId>,
    privileged: bool,
    candidates: &[(AgentId, Option<String>)],
    parents: &[(AgentId, Option<AgentId>)],
    subscribed: impl Fn(&str) -> bool,
) -> Vec<(AgentId, String)> {
    let addressed: Vec<AgentId> = e
        .fields
        .get("to")
        .map(|to| {
            to.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .filter_map(|t| atrium::ctl::resolve_target(t, candidates).ok())
                .collect()
        })
        .unwrap_or_default();
    let text = wake_text(e, who);
    let mut out: Vec<(AgentId, String)> = Vec::new();
    for (id, role) in candidates {
        let label = pane_label(*id, role.as_deref());
        if label == who || Some(*id) == caller || out.iter().any(|(o, _)| o == id) {
            continue;
        }
        let is_sub = subscribed(&label);
        if !(addressed.contains(id) || is_sub) {
            continue;
        }
        let allowed = privileged
            || is_sub
            || caller.is_some_and(|c| {
                atrium::ctl::in_subtree(*id, c, parents) || atrium::ctl::in_subtree(c, *id, parents)
            });
        if allowed {
            out.push((*id, text.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests;
