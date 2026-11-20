//! Search results for a person at a terminal: sessions, best first, with their
//! matching messages and the command that resumes each one.

use crate::query::{Answer, Mode};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const MARK: &str = "\x1b[1;33m";
const RESET: &str = "\x1b[0m";

pub fn print(a: &Answer) {
    if let Some(tok) = &a.refused {
        println!("Can't use the filter {BOLD}{tok}{RESET}: nothing searched.");
        println!("{DIM}Filters: in:<folder>  from:me|agent|file  after:/before:2026-11-01|7d  session:<id>{RESET}");
        return;
    }
    if a.sessions.is_empty() {
        println!("No matches for {BOLD}{}{RESET}.", a.query.trim());
        return;
    }
    let fuzzy = if a.mode == Some(Mode::Fuzzy) {
        " (no exact matches; allowing typos)"
    } else {
        ""
    };
    println!(
        "{DIM}{} matching messages, {} sessions shown{fuzzy}{RESET}\n",
        a.total,
        a.sessions.len()
    );
    for s in &a.sessions {
        println!(
            "{BOLD}{}{RESET}  {DIM}{}  {} match{}{RESET}",
            short_path(&s.project),
            when(s.last_ts),
            s.matches,
            if s.matches == 1 { "" } else { "es" }
        );
        println!(
            "  {DIM}cd {} && claude --resume {}{RESET}",
            shell_quote(&s.project),
            s.session
        );
        for h in &s.hits {
            let who = match h.role.as_str() {
                "user" => "you  ",
                "assistant" => "agent",
                "attachment" => "file ",
                other => other,
            };
            println!("  {DIM}{who}{RESET}  {}", marked(&h.snippet, &h.highlights));
        }
        println!();
    }
}

/// The snippet with its highlights (UTF-16 offsets) in bold.
fn marked(snippet: &str, highlights: &[[usize; 2]]) -> String {
    let mut out = String::with_capacity(snippet.len() + highlights.len() * 12);
    let mut at16 = 0usize;
    for c in snippet.chars() {
        if highlights.iter().any(|h| h[0] == at16) {
            out.push_str(MARK);
        }
        out.push(c);
        at16 += c.len_utf16();
        if highlights.iter().any(|h| h[1] == at16) {
            out.push_str(RESET);
        }
    }
    out
}

/// The last two folders of a path: enough to recognise a project.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match parts.len() {
        0 => p.to_owned(),
        1 | 2 => parts.join("/"),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Unix milliseconds as `2026-11-19 21:45 UTC`.
fn when(ms: i64) -> String {
    if ms <= 0 {
        return String::from("unknown time");
    }
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(when(1_795_146_763_697), "2026-11-20 03:52 UTC");
        assert_eq!(when(951_782_400_000), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn highlights_by_utf16_offset() {
        // "é" is one UTF-16 unit, "😀" two.
        assert_eq!(
            marked("é😀 hook", &[[4, 8]]),
            format!("é😀 {MARK}hook{RESET}")
        );
    }

    #[test]
    fn paths_and_quoting() {
        assert_eq!(short_path("/home/me/code/app/src"), "…/app/src");
        assert_eq!(short_path("/w"), "w");
        assert_eq!(shell_quote("/a b/it's"), "'/a b/it'\\''s'");
        assert_eq!(shell_quote("/w/app-src"), "/w/app-src");
    }
}
