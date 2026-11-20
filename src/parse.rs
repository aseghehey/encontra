//! One Claude Code session log line (JSONL) into the conversation text it holds.
//!
//! Conversation is what's kept: the person's prompts (typed, queued mid-turn,
//! slash commands, `!` shell commands) and the agent's replies. Besides that,
//! only the names of files the person attached (`@path`) are kept, so they can
//! be found by name; never their contents. Everything else is left
//! out: tool calls and their results, thinking, command output, sub-agent turns,
//! injected context (system reminders, task notifications), hook output and the
//! log's own bookkeeping (API request records, queue operations, attachments).
//!
//! What counts as a prompt follows herdr-chat's own reader (`mods/chat/parse.js`),
//! so search finds what the chat shows.

use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// A file the person attached; the piece's text is its path.
    Attachment,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Attachment => "attachment",
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

/// Cheap check before parsing: lines that can't hold conversation are skipped
/// without building a JSON tree (most of a long log is bookkeeping and tool
/// traffic).
fn may_hold_message(raw: &str) -> bool {
    let user = raw.contains("\"type\":\"user\"");
    if user && raw.contains("\"tool_result\"") && !raw.contains("\"type\":\"text\"") {
        return false; // a tool result and nothing the person wrote
    }
    user || raw.contains("\"type\":\"assistant\"")
        || raw.contains("\"local_command\"")
        || raw.contains("\"queued_command\"")
        || (raw.contains("\"attachment\"") && raw.contains("\"type\":\"file\""))
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
    match d.get("type").and_then(Value::as_str).unwrap_or("") {
        "user" => user_line(&d, &mut line.pieces),
        "assistant" => assistant_line(&d, &mut line.pieces),
        "system" => {
            if d.get("subtype").and_then(Value::as_str) == Some("local_command") {
                if let Some(c) = d.get("content").and_then(Value::as_str) {
                    user_text(c, &mut line.pieces);
                }
            }
        }
        "attachment" => attachment(&d, &mut line.pieces),
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
    match d.pointer("/message/content") {
        Some(Value::String(s)) => user_text(s, out),
        // Text blocks only: tool results ride in the same message but aren't
        // anything the person wrote.
        Some(Value::Array(blocks)) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
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
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !text.is_empty() {
        out.push(Piece {
            role: Role::Assistant,
            text,
        });
    }
}

/// Attachment lines that matter: files the person attached, and messages they
/// sent while the agent was busy (logged as queued commands). Only the
/// person's own prompts count, not commands or other machines' messages.
fn attachment(d: &Value, out: &mut Vec<Piece>) {
    let Some(a) = d.get("attachment") else { return };
    match a.get("type").and_then(Value::as_str) {
        Some("file") => {
            if let Some(name) = a
                .get("filename")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
            {
                out.push(Piece {
                    role: Role::Attachment,
                    text: name.trim().to_owned(),
                });
            }
            return;
        }
        Some("queued_command") => {}
        _ => return,
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
    bash_in: regex::Regex,
    wrapped: regex::Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        reminder: regex::Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap(),
        command: regex::Regex::new(r"<command-name>([^<]*)</command-name>").unwrap(),
        args: regex::Regex::new(r"(?s)<command-args>(.*?)</command-args>").unwrap(),
        bash_in: regex::Regex::new(r"(?s)<bash-input>(.*?)</bash-input>").unwrap(),
        // A whole message that is one tag-wrapped injection (task notifications,
        // environment context, command or shell output). The regex crate has no
        // backreferences, so the closing tag is compared by hand.
        wrapped: regex::Regex::new(r"(?s)^<([a-z_-]+)[^>]*>.*</([a-z_-]+)>$").unwrap(),
    })
}

/// The person's text as the chat shows it, or nothing for anything else.
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
    if let Some(c) = p.bash_in.captures(text) {
        return out.push(Piece {
            role: Role::User,
            text: format!("! {}", c[1].trim()),
        });
    }
    if text.starts_with("[Request interrupted") {
        return;
    }
    // Command and shell output (<local-command-stdout>, <bash-stdout>), caveats
    // and other injections are one wrapped block each.
    if let Some(c) = p.wrapped.captures(text) {
        if c[1] == c[2] {
            return;
        }
    }
    if text.starts_with("<local-command-") || text.starts_with("<bash-std") {
        return;
    }
    if !text.is_empty() {
        out.push(Piece {
            role: Role::User,
            text: text.to_owned(),
        });
    }
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
        for injected in [
            r#"{"type":"user","message":{"content":"<task-notification><id>1</id></task-notification>"}}"#,
            r#"{"type":"user","message":{"content":"<local-command-caveat>Caveat</local-command-caveat>"}}"#,
            r#"{"type":"user","message":{"content":"<local-command-stdout>Set model to Opus</local-command-stdout>"}}"#,
            r#"{"type":"user","message":{"content":"<bash-stdout>total 48</bash-stdout><bash-stderr></bash-stderr>"}}"#,
            r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#,
        ] {
            assert!(texts(injected).is_empty(), "{injected}");
        }
    }

    #[test]
    fn meta_sidechain_and_bookkeeping_are_skipped() {
        assert!(texts(r#"{"type":"user","isMeta":true,"message":{"content":"x"}}"#).is_empty());
        assert!(
            texts(r#"{"type":"user","isSidechain":true,"message":{"content":"x"}}"#).is_empty()
        );
        assert!(
            texts(r#"{"type":"api-request-blob","message":{"role":"user","content":"x"}}"#)
                .is_empty()
        );
        assert!(texts(r#"{"type":"attachment","attachment":{"type":"hook_success","content":"queued_command"}}"#).is_empty());
    }

    #[test]
    fn slash_and_shell_commands_read_as_typed() {
        let raw = r#"{"type":"user","message":{"content":"<command-name>/model</command-name><command-message>model</command-message><command-args>opus</command-args>"}}"#;
        assert_eq!(texts(raw), vec![(Role::User, "/model opus".into())]);
        let bash = r#"{"type":"user","message":{"content":"<bash-input>git status</bash-input>"}}"#;
        assert_eq!(texts(bash), vec![(Role::User, "! git status".into())]);
    }

    #[test]
    fn replies_without_thinking_or_tool_calls() {
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Found it."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}},{"type":"text","text":"Fixed."}]}}"#;
        assert_eq!(
            texts(raw),
            vec![(Role::Assistant, "Found it.\n\nFixed.".into())]
        );
        let only_tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;
        assert!(texts(only_tool).is_empty());
    }

    #[test]
    fn tool_results_are_not_conversation() {
        let plain = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        assert!(texts(plain).is_empty());
        let nested = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"file contents"}]}]}}"#;
        assert!(texts(nested).is_empty());
        let beside = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"},{"type":"text","text":"and also this"}]}}"#;
        assert_eq!(texts(beside), vec![(Role::User, "and also this".into())]);
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
    }

    #[test]
    fn attached_files_by_name_only() {
        let file = r#"{"type":"attachment","attachment":{"type":"file","filename":"/tmp/api/API-TRACE.md","displayPath":"API-TRACE.md","content":{"file":{"content":"secret body"}}}}"#;
        assert_eq!(
            texts(file),
            vec![(Role::Attachment, "/tmp/api/API-TRACE.md".into())]
        );
        for other in [
            r#"{"type":"attachment","attachment":{"type":"compact_file_reference","filename":"/tmp/a.md"}}"#,
            r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/tmp/b.md","snippet":""}}"#,
            r#"{"type":"attachment","isSidechain":true,"attachment":{"type":"file","filename":"/tmp/c.md"}}"#,
        ] {
            assert!(texts(other).is_empty(), "{other}");
        }
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
}
