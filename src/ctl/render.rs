//! Human views of board and bus replies, for a terminal.

use json::Value;

/// Render a `board` reply as a human view: one line per entry, the `status`
/// field colored (when `color` is true), `http(s)` values as OSC-8 clickable
/// links, and the writer dim in parens. `None` if the reply isn't a board
/// result (caller falls back to the raw JSON).
pub(super) fn render_board(reply: &str, color: bool) -> Option<String> {
    let v = json::parse(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // list → a table of entries (each carrying its own inline "key").
    if let Some(arr) = v.get("board").and_then(Value::as_array) {
        if arr.is_empty() {
            return Some("  (board is empty)".to_string());
        }
        let body = arr
            .iter()
            .map(|e| {
                let key = e.get("key").and_then(Value::as_str).unwrap_or("?");
                render_entry_line(key, e, color)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(body);
    }
    // del → a one-line confirmation.
    if let Some(deleted) = v.get("deleted").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        return Some(format!(
            "  {key}: {}",
            if deleted {
                "removed"
            } else {
                "was not on the board"
            }
        ));
    }
    // claim → granted shows the now-held entry; denied names the current holder.
    if let Some(granted) = v.get("granted").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        if granted {
            let line = match v.get("entry") {
                Some(entry) if entry != &Value::Null => render_entry_line(key, entry, color),
                _ => format!("  {key}"),
            };
            let suffix = if color {
                format!("\n  \x1b[38;5;10mclaimed {key}\x1b[0m")
            } else {
                format!("\n  claimed {key}")
            };
            return Some(format!("{line}{suffix}"));
        }
        let holder = v.get("holder").and_then(Value::as_str).unwrap_or("someone");
        return Some(if color {
            format!("  \x1b[38;5;11m{key} is held by {holder}\x1b[0m — pick another task")
        } else {
            format!("  {key} is held by {holder} — pick another task")
        });
    }
    // release → a one-line confirmation.
    if let Some(released) = v.get("released").and_then(Value::as_bool) {
        let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
        return Some(format!(
            "  {key}: {}",
            if released {
                "released"
            } else {
                "was not on the board"
            }
        ));
    }
    // get/set → one entry (or "not on the board").
    if let Some(key) = v.get("key").and_then(Value::as_str) {
        return Some(match v.get("entry") {
            Some(Value::Null) | None => format!("  {key}: (not on the board)"),
            Some(entry) => render_entry_line(key, entry, color),
        });
    }
    None
}

/// Render a `bus` reply as a human view: a `feed`/`pub` shows one line per event
/// (seq, an urgency glyph, the topic, its fields, and who sent it); `sub`/`unsub`
/// shows the resulting subscription set; `resolve` a one-line confirmation. `None`
/// if the reply isn't a bus result (caller falls back to raw JSON).
pub(super) fn render_bus(reply: &str, color: bool) -> Option<String> {
    let v = json::parse(reply).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // feed → a list of events (plus the cursor to resume from).
    if let Some(arr) = v.get("feed").and_then(Value::as_array) {
        if arr.is_empty() {
            return Some("  (no new events)".to_string());
        }
        let cursor = v.get("cursor").and_then(Value::as_i64).unwrap_or(0);
        let mut body = arr
            .iter()
            .map(|e| render_event_line(e, color))
            .collect::<Vec<_>>()
            .join("\n");
        body.push_str(&if color {
            format!("\n\x1b[2m  — cursor {cursor} (next: bus feed --since {cursor})\x1b[0m")
        } else {
            format!("\n  — cursor {cursor} (next: bus feed --since {cursor})")
        });
        return Some(body);
    }
    // topics → the active-topic roster with subscriber counts.
    if let Some(arr) = v.get("topics").and_then(Value::as_array) {
        if arr.is_empty() {
            return Some("  (no active topics)".to_string());
        }
        let body = arr
            .iter()
            .map(|t| {
                let topic = t.get("topic").and_then(Value::as_str).unwrap_or("?");
                let subs = t.get("subs").and_then(Value::as_i64).unwrap_or(0);
                if color {
                    format!("  \x1b[1m{topic}\x1b[0m  subs={subs}")
                } else {
                    format!("  {topic}  subs={subs}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(body);
    }
    // pub → the single stored event. The zero-subscriber warning is deliberately
    // NOT rendered here: stdout carries the JSON reply that consumers `json::parse`,
    // so the warning goes to STDERR via [`zero_sub_warning`] in the client path
    // ([`ctl_cmd`]) — mixing it into stdout would corrupt every existing `bus pub`
    // consumer. The additive `subscribers` field in the reply is ignored here.
    if let Some(event) = v.get("event") {
        return Some(render_event_line(event, color));
    }
    // sub/unsub → the resulting subscription set.
    if let Some(subs) = v.get("subscribed").and_then(Value::as_array) {
        if subs.is_empty() {
            return Some("  (subscribed to nothing)".to_string());
        }
        let list = subs
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("  subscribed: {list}"));
    }
    // resolve → a one-line confirmation.
    if let Some(resolved) = v.get("resolved").and_then(Value::as_bool) {
        let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
        return Some(format!(
            "  decision #{seq}: {}",
            if resolved {
                "resolved"
            } else {
                "was not an open decision"
            }
        ));
    }
    None
}

/// Format one bus event: `[#7] ! deploy  msg=ship it?  (from dev_1)`. A
/// `decision_needed` event gets an amber `!` and bold topic so escalations stand
/// out from FYI chatter (a dim `·`). SGR codes are emitted only when `color` is true.
fn render_event_line(event: &Value, color: bool) -> String {
    let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let topic = event.get("topic").and_then(Value::as_str).unwrap_or("?");
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("fyi");
    let decision = kind == "decision_needed";
    let (seq_prefix, seq_suffix, glyph, topic_prefix, topic_suffix) = if color {
        if decision {
            (
                "\x1b[2m",
                "\x1b[0m",
                "\x1b[38;5;11m!\x1b[0m",
                "\x1b[1;38;5;11m",
                "\x1b[0m",
            )
        } else {
            (
                "\x1b[2m",
                "\x1b[0m",
                "\x1b[2m·\x1b[0m",
                "\x1b[1m",
                "\x1b[0m",
            )
        }
    } else if decision {
        ("", "", "!", "", "")
    } else {
        ("", "", "·", "", "")
    };
    let empty: &[(String, Value)] = &[];
    let fields = event
        .get("fields")
        .and_then(Value::as_object)
        .unwrap_or(empty);
    let field_str = fields
        .iter()
        .map(|(k, v)| {
            let vs = v.as_str().unwrap_or("");
            if is_url(vs) {
                if color {
                    format!("{k}={}", hyperlink(vs, vs))
                } else {
                    format!("{k}={vs}")
                }
            } else {
                format!("{k}={vs}")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let from = event
        .get("from")
        .and_then(Value::as_str)
        .map(|f| {
            if color {
                format!("  \x1b[2m(from {f})\x1b[0m")
            } else {
                format!("  (from {f})")
            }
        })
        .unwrap_or_default();
    format!("{seq_prefix}[#{seq}]{seq_suffix} {glyph} {topic_prefix}{topic}{topic_suffix}  {field_str}{from}")
}

/// Format one board entry: `● key   status=DONE  owner=Max  url=<link>  (by dev_1)`.
/// `entry` is `{by, ms, fields:{…}}`; the `key` is passed in (list entries carry it
/// inline, get/set entries don't). SGR codes are emitted only when `color` is true.
fn render_entry_line(key: &str, entry: &Value, color: bool) -> String {
    let empty: &[(String, Value)] = &[];
    let fields = entry
        .get("fields")
        .and_then(Value::as_object)
        .unwrap_or(empty);
    let status = fields
        .iter()
        .find(|(k, _)| k == "status")
        .and_then(|(_, v)| v.as_str());
    let glyph = match status {
        Some(st) if color => format!("{}\u{25CF}\x1b[0m ", status_sgr(st)),
        Some(_) => "\u{25CF} ".to_string(),
        None => "  ".to_string(),
    };
    let field_str = fields
        .iter()
        .map(|(k, v)| {
            let vs = v.as_str().unwrap_or("");
            let rendered = if k == "status" {
                if color {
                    format!("{}{vs}\x1b[0m", status_sgr(vs))
                } else {
                    vs.to_string()
                }
            } else if is_url(vs) {
                if color {
                    hyperlink(vs, vs)
                } else {
                    vs.to_string()
                }
            } else {
                vs.to_string()
            };
            format!("{k}={rendered}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    // A live claim shows a `⊙holder` tag so a glance at the board tells
    // you who is actively working each task (empty when unclaimed).
    let held = entry
        .get("claimed_by")
        .and_then(Value::as_str)
        .map(|h| {
            if color {
                format!("  \x1b[38;5;13m\u{2299}{h}\x1b[0m")
            } else {
                format!("  \u{2299}{h}")
            }
        })
        .unwrap_or_default();
    let by = entry
        .get("by")
        .and_then(Value::as_str)
        .map(|b| {
            if color {
                format!("  \x1b[2m(by {b})\x1b[0m")
            } else {
                format!("  (by {b})")
            }
        })
        .unwrap_or_default();
    if color {
        format!("{glyph}\x1b[1m{key:<12}\x1b[0m {field_str}{held}{by}")
    } else {
        format!("{glyph}{key:<12} {field_str}{held}{by}")
    }
}

/// A status string → an SGR color: green done/shipped, red blocked/failed, cyan
/// in-progress, amber waiting/todo, default otherwise. Case-insensitive substring.
/// Shared by the CLI board view and the in-chrome board panel.
pub fn status_sgr(status: &str) -> &'static str {
    let s = status.to_ascii_lowercase();
    if s.contains("done") || s.contains("ship") || s.contains("complete") || s == "ok" {
        "\x1b[38;5;10m" // green
    } else if s.contains("block") || s.contains("fail") || s.contains("stuck") {
        "\x1b[38;5;9m" // red
    } else if s.contains("wip") || s.contains("progress") || s.contains("working") {
        "\x1b[38;5;14m" // cyan
    } else if s.contains("wait") || s.contains("todo") || s.contains("pending") {
        "\x1b[38;5;11m" // amber
    } else {
        "\x1b[0m"
    }
}

/// A value that looks like a web link.
pub fn is_url(v: &str) -> bool {
    v.starts_with("http://") || v.starts_with("https://")
}

/// Wrap `url` as an OSC-8 hyperlink (clickable in modern terminals). `label` is
/// the visible text (pass the url itself to show it verbatim).
pub fn hyperlink(url: &str, label: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

/// A one-glyph status marker for a status string: `●` done, `○` blocked, `◐`
/// in-progress/waiting, `·` otherwise. Pair with [`status_sgr`] for color.
pub fn status_glyph(status: &str) -> &'static str {
    let s = status.to_ascii_lowercase();
    if s.contains("done") || s.contains("ship") || s.contains("complete") || s == "ok" {
        "\u{25CF}" // ●
    } else if s.contains("block") || s.contains("fail") || s.contains("stuck") {
        "\u{25CB}" // ○
    } else if s.contains("wip")
        || s.contains("progress")
        || s.contains("working")
        || s.contains("wait")
        || s.contains("todo")
        || s.contains("pending")
    {
        "\u{25D0}" // ◐
    } else {
        "\u{00B7}" // ·
    }
}

#[cfg(test)]
mod tests;
