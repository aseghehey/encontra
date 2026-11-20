//! One Claude Code session log line (JSONL) into the searchable pieces it holds.
//!
//! What counts as a message follows herdr-chat's own reader (`mods/chat/parse.js`),
//! so search finds what the chat shows: the person's prompts (typed, queued
//! mid-turn, or slash commands), the agent's replies, and tool calls with their
//! results. Sub-agent turns, injected context, hook output and the log's own
//! bookkeeping (API request records, queue operations, attachments) are left out.

use serde_json::Value;
use std::sync::OnceLock;

/// Tool inputs and results are cut to this many bytes: enough to find an error
/// message or a command, without indexing whole files the agent read.
pub const MAX_TOOL_TEXT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Line {
    pub session: Option<String>,
    pub cwd: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub ts: Option<i64>,
    pub pieces: Vec<Piece>,
}

/// Cheap check before parsing: lines that can't hold a message are skipped
/// without building a JSON tree (most of a long log is bookkeeping).
fn may_hold_message(raw: &str) -> bool {
    raw.contains("\"type\":\"user\"")
        || raw.contains("\"type\":\"assistant\"")
        || raw.contains("\"local_command\"")
        || raw.contains("\"queued_command\"")
}

pub fn parse_line(raw: &str) -> Option<Line> {
    if !may_hold_message(raw) {
        return None;
    }
    let d: Value = serde_json::from_str(raw).ok()?;
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let mut line = Line {
        session: d
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: d.get("cwd").and_then(Value::as_str).map(str::to_owned),
        ts: d
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_millis),
        pieces: Vec::new(),
    };
    let kind = d.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "user" => user_line(&d, &mut line.pieces),
        "assistant" => assistant_line(&d, &mut line.pieces),
        "system" => {
            if d.get("subtype").and_then(Value::as_str) == Some("local_command") {
                if let Some(c) = d.get("content").and_then(Value::as_str) {
                    user_text(c, &mut line.pieces);
                }
            }
        }
        "attachment" => queued_command(&d, &mut line.pieces),
        _ => {}
    }
    if line.pieces.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn user_line(d: &Value, out: &mut Vec<Piece>) {
    for flag in ["isMeta", "isCompactSummary", "isVisibleInTranscriptOnly"] {
        if d.get(flag).and_then(Value::as_bool) == Some(true) {
            return;
        }
    }
    let Some(content) = d.pointer("/message/content") else {
        return;
    };
    match content {
        Value::String(s) => user_text(s, out),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            texts.push(t);
                        }
                    }
                    Some("tool_result") => {
                        let t = to_text(b.get("content").unwrap_or(&Value::Null));
                        push_tool(out, t);
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() {
                user_text(&texts.join("\n"), out);
            }
        }
        _ => {}
    }
}

fn assistant_line(d: &Value, out: &mut Vec<Piece>) {
    let Some(Value::Array(blocks)) = d.pointer("/message/content") else {
        return;
    };
    let mut text = String::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                }
            }
            Some("tool_use") => {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = b.get("input").map(Value::to_string).unwrap_or_default();
                push_tool(out, format!("{name} {input}"));
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        out.push(Piece {
            role: Role::Assistant,
            text,
        });
    }
}

/// Messages sent while the agent was busy are logged as attachments. Only the
/// person's own prompts count, not commands or other machines' messages.
fn queued_command(d: &Value, out: &mut Vec<Piece>) {
    let Some(a) = d.get("attachment") else { return };
    if a.get("type").and_then(Value::as_str) != Some("queued_command") {
        return;
    }
    let origin = a
        .pointer("/origin/kind")
        .and_then(Value::as_str)
        .unwrap_or("human");
    let mode = a
        .get("commandMode")
        .and_then(Value::as_str)
        .unwrap_or("prompt");
    if origin != "human" || mode != "prompt" {
        return;
    }
    match a.get("prompt") {
        Some(Value::String(s)) => user_text(s, out),
        Some(Value::Array(blocks)) => {
            let t: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            user_text(&t.join("\n"), out);
        }
        _ => {}
    }
}

struct Patterns {
    reminder: regex::Regex,
    command: regex::Regex,
    args: regex::Regex,
    local_out: regex::Regex,
    bash_in: regex::Regex,
    bash_out: regex::Regex,
    wrapped: regex::Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        reminder: regex::Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap(),
        command: regex::Regex::new(r"<command-name>([^<]*)</command-name>").unwrap(),
        args: regex::Regex::new(r"(?s)<command-args>(.*?)</command-args>").unwrap(),
        local_out: regex::Regex::new(
            r"(?s)<local-command-(?:stdout|stderr)>(.*?)</local-command-(?:stdout|stderr)>",
        )
        .unwrap(),
        bash_in: regex::Regex::new(r"(?s)<bash-input>(.*?)</bash-input>").unwrap(),
        bash_out: regex::Regex::new(r"(?s)<bash-stdout>(.*?)</bash-stdout>").unwrap(),
        // A whole message that is one tag-wrapped injection (task notifications,
        // environment context). The regex crate has no backreferences, so the
        // closing tag is compared by hand.
        wrapped: regex::Regex::new(r"(?s)^<([a-z_-]+)[^>]*>.*</([a-z_-]+)>$").unwrap(),
    })
}

/// The person's text as the chat shows it, or nothing for injected content.
fn user_text(raw: &str, out: &mut Vec<Piece>) {
    let p = patterns();
    let text = p.reminder.replace_all(raw, "");
    let text = text.trim();
    if let Some(c) = p.command.captures(text) {
        let name = c[1].trim();
        let args = p
            .args
            .captures(text)
            .map(|a| a[1].trim().to_owned())
            .unwrap_or_default();
        let t = if args.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {args}")
        };
        return out.push(Piece {
            role: Role::User,
            text: t,
        });
    }
    if let Some(c) = p.local_out.captures(text) {
        return push_tool(out, strip_ansi(c[1].trim()));
    }
    if text.starts_with("<local-command-caveat>") || text.starts_with("[Request interrupted") {
        return;
    }
    if let Some(c) = p.bash_in.captures(text) {
        return out.push(Piece {
            role: Role::User,
            text: format!("! {}", c[1].trim()),
        });
    }
    if let Some(c) = p.bash_out.captures(text) {
        return push_tool(out, strip_ansi(c[1].trim()));
    }
    if let Some(c) = p.wrapped.captures(text) {
        if c[1] == c[2] {
            return;
        }
    }
    if !text.is_empty() {
        out.push(Piece {
            role: Role::User,
            text: text.to_owned(),
        });
    }
}

fn push_tool(out: &mut Vec<Piece>, text: String) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    out.push(Piece {
        role: Role::Tool,
        text: truncate(text, MAX_TOOL_TEXT).to_owned(),
    });
}

/// Anything content-like (a string, `[{text}]`, `{content}`) as plain text.
fn to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(o) => {
            if let Some(Value::String(t)) = o.get("text") {
                t.clone()
            } else if let Some(c) = o.get("content") {
                to_text(c)
            } else if let Some(c) = o.get("output") {
                to_text(c)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// At most `max` bytes, cut on a character boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn strip_ansi(s: &str) -> String {
    static ANSI: OnceLock<regex::Regex> = OnceLock::new();
    ANSI.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap())
        .replace_all(s, "")
        .into_owned()
}

/// `2026-11-20T03:52:43.697Z` (or with an offset) as Unix milliseconds.
pub fn parse_iso_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let digits = &frac[..frac.len().min(3)];
        ms = format!("{digits:0<3}").parse().ok()?;
    }
    let offset_min = match b.get(i) {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&c) if c == b'+' || c == b'-' => {
            let oh = num(i + 1..i + 3)?;
            let om = s
                .get(i + 3..)
                .map(|r| r.trim_start_matches(':'))
                .and_then(|r| r.get(0..2)?.parse::<i64>().ok())
                .unwrap_or(0);
            let sign = if c == b'+' { 1 } else { -1 };
            sign * (oh * 60 + om)
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y2, m2) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    Some(secs * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(raw: &str) -> Vec<(Role, String)> {
        parse_line(raw)
            .map(|l| l.pieces.into_iter().map(|p| (p.role, p.text)).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_typed_prompt() {
        let raw = r#"{"type":"user","sessionId":"s1","cwd":"/w","timestamp":"2026-11-20T03:52:43.697Z","message":{"role":"user","content":"Fix the login bug"}}"#;
        let l = parse_line(raw).unwrap();
        assert_eq!(l.session.as_deref(), Some("s1"));
        assert_eq!(l.cwd.as_deref(), Some("/w"));
        assert_eq!(l.ts, Some(1_795_146_763_697));
        assert_eq!(
            l.pieces,
            vec![Piece {
                role: Role::User,
                text: "Fix the login bug".into()
            }]
        );
    }

    #[test]
    fn reminders_are_removed_and_injections_dropped() {
        let raw = r#"{"type":"user","message":{"content":"<system-reminder>noise</system-reminder>\nreal question"}}"#;
        assert_eq!(texts(raw), vec![(Role::User, "real question".into())]);
        let injected = r#"{"type":"user","message":{"content":"<task-notification><id>1</id></task-notification>"}}"#;
        assert!(texts(injected).is_empty());
        let caveat = r#"{"type":"user","message":{"content":"<local-command-caveat>Caveat</local-command-caveat>"}}"#;
        assert!(texts(caveat).is_empty());
    }

    #[test]
    fn meta_and_sidechain_lines_are_skipped() {
        assert!(texts(r#"{"type":"user","isMeta":true,"message":{"content":"x"}}"#).is_empty());
        assert!(
            texts(r#"{"type":"user","isSidechain":true,"message":{"content":"x"}}"#).is_empty()
        );
        assert!(
            texts(r#"{"type":"api-request-blob","message":{"role":"user","content":"x"}}"#)
                .is_empty()
        );
    }

    #[test]
    fn slash_commands_read_as_typed() {
        let raw = r#"{"type":"user","message":{"content":"<command-name>/model</command-name><command-message>model</command-message><command-args>opus</command-args>"}}"#;
        assert_eq!(texts(raw), vec![(Role::User, "/model opus".into())]);
    }

    #[test]
    fn replies_and_tool_calls() {
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Found it."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        assert_eq!(
            texts(raw),
            vec![
                (Role::Tool, r#"Bash {"command":"cargo test"}"#.into()),
                (Role::Assistant, "Found it.".into())
            ]
        );
    }

    #[test]
    fn tool_results_are_cut() {
        let long = "e".repeat(MAX_TOOL_TEXT * 2);
        let raw = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":[{{"type":"text","text":"{long}"}}]}}]}}}}"#
        );
        let t = texts(&raw);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, Role::Tool);
        assert_eq!(t[0].1.len(), MAX_TOOL_TEXT);
    }

    #[test]
    fn queued_prompts_from_the_person_only() {
        let human = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"also check the docs"}}"#;
        assert_eq!(
            texts(human),
            vec![(Role::User, "also check the docs".into())]
        );
        let other = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"x","origin":{"kind":"agent"}}}"#;
        assert!(texts(other).is_empty());
        let hook = r#"{"type":"attachment","attachment":{"type":"hook_success","content":"queued_command"}}"#;
        assert!(texts(hook).is_empty());
    }

    #[test]
    fn iso_times() {
        assert_eq!(parse_iso_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_millis("2026-11-20T03:52:43.697Z"),
            Some(1_795_146_763_697)
        );
        assert_eq!(
            parse_iso_millis("2026-11-20T04:52:43.697+01:00"),
            Some(1_795_146_763_697)
        );
        assert_eq!(
            parse_iso_millis("2026-11-20T03:52:43.6Z"),
            Some(1_795_146_763_600)
        );
        assert_eq!(parse_iso_millis("nope"), None);
    }

    #[test]
    fn truncate_respects_characters() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
    }
}
