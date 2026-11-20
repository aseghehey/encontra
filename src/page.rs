//! A stretch of a session log as the chat needs it: whole lines with their byte
//! offsets, without the log's bookkeeping, and with long tool output cut.
//!
//! The chat reads the end of a log itself. To show earlier history, or to open
//! at a search hit far back in a long log, it asks for the lines in a byte
//! range instead: most of a long log is bookkeeping it never shows, so this is
//! a small fraction of the bytes.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Tool output beyond this many characters is cut (the chat shows less).
const MAX_RESULT: usize = 6000;
/// Never more than this many bytes of a log in one page.
pub const MAX_SPAN: u64 = 256 * 1024 * 1024;

/// Line types the chat never shows.
const DROP: &[&str] = &[
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "cost-state",
    "atis-latch",
    "tools-used",
    "last-prompt",
    "mode",
    "permission-mode",
    "file-history-snapshot",
];

#[derive(Serialize)]
pub struct Line {
    pub at: u64,
    pub line: String,
}

#[derive(Serialize)]
pub struct Page {
    /// Where the first line returned (or skipped) starts.
    pub from: u64,
    /// Just past the last whole line read.
    pub to: u64,
    pub size: u64,
    pub lines: Vec<Line>,
}

/// The lines that start in `[from, to)`, `from` moved forward to a line start.
pub fn page(path: &Path, from: u64, to: Option<u64>) -> Result<Page> {
    let mut f = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let size = f.metadata()?.len();
    let to = to.unwrap_or(size).min(size);
    let mut from = from.min(to);
    if to - from > MAX_SPAN {
        from = to - MAX_SPAN;
    }
    // A start inside a line moves to the next line.
    if from > 0 {
        f.seek(SeekFrom::Start(from - 1))?;
        let mut b = [0u8; 1];
        f.read_exact(&mut b)?;
        if b[0] != b'\n' {
            let mut buf = Vec::new();
            (&mut f).take(to - from).read_to_end(&mut buf)?;
            match buf.iter().position(|&c| c == b'\n') {
                Some(i) => from += i as u64 + 1,
                None => from = to,
            }
        }
    }
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    (&mut f).take(to - from).read_to_end(&mut buf)?;
    // The last line may run past `to`: read on to its end.
    if to < size && buf.last() != Some(&b'\n') {
        let mut more = [0u8; 64 * 1024];
        loop {
            let n = f.read(&mut more)?;
            if n == 0 {
                break;
            }
            if let Some(i) = more[..n].iter().position(|&c| c == b'\n') {
                buf.extend_from_slice(&more[..=i]);
                break;
            }
            buf.extend_from_slice(&more[..n]);
        }
    }
    let end = match buf.iter().rposition(|&c| c == b'\n') {
        Some(i) => i + 1,
        None => 0,
    };
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &c) in buf[..end].iter().enumerate() {
        if c != b'\n' {
            continue;
        }
        let at = from + start as u64;
        let raw = &buf[start..i];
        start = i + 1;
        if let Some(line) = std::str::from_utf8(raw).ok().and_then(slim) {
            lines.push(Line { at, line });
        }
    }
    Ok(Page {
        from,
        to: from + end as u64,
        size,
        lines,
    })
}

/// The line as the chat needs it, or nothing for lines it never shows.
fn slim(raw: &str) -> Option<String> {
    let mut d: Value = serde_json::from_str(raw).ok()?;
    let kind = d.get("type").and_then(Value::as_str).unwrap_or("");
    if DROP.contains(&kind) || d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if kind == "attachment"
        && d.pointer("/attachment/type").and_then(Value::as_str) != Some("queued_command")
    {
        return None;
    }
    let obj = d.as_object_mut()?;
    // The full tool result again, for the CLI's own use; the message has it.
    obj.remove("toolUseResult");
    if let Some(Value::Array(blocks)) = obj.get_mut("message").and_then(|m| m.get_mut("content")) {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(c) = b.get_mut("content") {
                    cut(c);
                }
            }
        }
    }
    serde_json::to_string(&d).ok()
}

fn cut(v: &mut Value) {
    match v {
        Value::String(s) if s.chars().count() > MAX_RESULT => {
            *s = s.chars().take(MAX_RESULT).collect();
        }
        Value::Array(a) => a.iter_mut().for_each(cut),
        Value::Object(o) => {
            if let Some(t) = o.get_mut("text") {
                cut(t);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn log(lines: &[String]) -> (std::path::PathBuf, Vec<u64>) {
        let p = std::env::temp_dir().join(format!(
            "herdr-search-page-{}-{}.jsonl",
            std::process::id(),
            lines.len()
        ));
        let mut f = File::create(&p).unwrap();
        let mut at = Vec::new();
        let mut off = 0u64;
        for l in lines {
            at.push(off);
            writeln!(f, "{l}").unwrap();
            off += l.len() as u64 + 1;
        }
        (p, at)
    }

    #[test]
    fn whole_lines_with_offsets_without_bookkeeping() {
        let big = "x".repeat(MAX_RESULT * 2);
        let lines = vec![
            r#"{"type":"user","message":{"content":"hello"}}"#.to_string(),
            r#"{"type":"api-request-blob","message":{"content":"noise"}}"#.to_string(),
            format!(
                r#"{{"type":"user","toolUseResult":{{"stdout":"{big}"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t","content":"{big}"}}]}}}}"#
            ),
            r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"also"}}"#
                .to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"bye"}]}}"#
                .to_string(),
        ];
        let (p, at) = log(&lines);
        let pg = page(&p, 0, None).unwrap();
        let got: Vec<u64> = pg.lines.iter().map(|l| l.at).collect();
        assert_eq!(got, vec![at[0], at[2], at[4], at[5]]);
        let tool: Value = serde_json::from_str(&pg.lines[1].line).unwrap();
        assert!(tool.get("toolUseResult").is_none());
        assert_eq!(
            tool.pointer("/message/content/0/content")
                .unwrap()
                .as_str()
                .unwrap()
                .len(),
            MAX_RESULT
        );
        assert_eq!(pg.to, pg.size);

        // A start inside a line moves to the next one; the end finishes its line.
        let mid = page(&p, at[0] + 3, Some(at[5] + 2)).unwrap();
        assert_eq!(mid.from, at[1]);
        assert_eq!(mid.lines.last().unwrap().at, at[5]);
        // A start exactly on a line keeps it.
        assert_eq!(page(&p, at[4], None).unwrap().lines[0].at, at[4]);
        let _ = std::fs::remove_file(&p);
    }
}

#[derive(Serialize)]
pub struct Follow {
    #[serde(flatten)]
    pub page: Page,
    /// The log got shorter than `from`: it was rewritten, read it again.
    pub reset: bool,
}

/// Waits (up to `wait`) until whole new lines follow `from` (a line start),
/// then returns them as `page` does; with nothing new, an empty page at `from`.
/// The chat calls this in a loop, so new messages show as soon as they're
/// written, without asking again and again.
pub fn follow(path: &Path, from: u64, wait: std::time::Duration) -> Result<Follow> {
    let until = std::time::Instant::now() + wait;
    loop {
        let size = std::fs::metadata(path)
            .with_context(|| format!("can't read {}", path.display()))?
            .len();
        if size < from {
            return Ok(Follow {
                page: Page {
                    from,
                    to: from,
                    size,
                    lines: vec![],
                },
                reset: true,
            });
        }
        if size > from {
            let p = page(path, from, None)?;
            // Only a half-written line so far: wait for the rest.
            if p.to > from {
                return Ok(Follow {
                    page: p,
                    reset: false,
                });
            }
        }
        if std::time::Instant::now() >= until {
            return Ok(Follow {
                page: Page {
                    from,
                    to: from,
                    size,
                    lines: vec![],
                },
                reset: false,
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

#[cfg(test)]
mod follow_tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    #[test]
    fn returns_new_lines_as_they_come_and_waits_for_whole_ones() {
        let p =
            std::env::temp_dir().join(format!("herdr-search-follow-{}.jsonl", std::process::id()));
        let first = r#"{"type":"user","message":{"content":"one"}}"#;
        std::fs::write(&p, format!("{first}\n")).unwrap();
        let end = first.len() as u64 + 1;
        // Nothing new: an empty page at the same place, after the wait.
        let f = follow(&p, end, Duration::from_millis(150)).unwrap();
        assert!(f.page.lines.is_empty() && !f.reset && f.page.to == end);
        // A half-written line isn't returned...
        let mut w = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        w.write_all(br#"{"type":"user","mess"#).unwrap();
        assert!(follow(&p, end, Duration::from_millis(150))
            .unwrap()
            .page
            .lines
            .is_empty());
        // ...until it's whole.
        w.write_all(b"age\":{\"content\":\"two\"}}\n").unwrap();
        let f = follow(&p, end, Duration::from_millis(150)).unwrap();
        assert_eq!(f.page.lines.len(), 1);
        assert_eq!(f.page.lines[0].at, end);
        // A rewritten (shorter) log says so.
        std::fs::write(&p, "{}\n").unwrap();
        assert!(follow(&p, end, Duration::from_millis(50)).unwrap().reset);
        let _ = std::fs::remove_file(&p);
    }
}
