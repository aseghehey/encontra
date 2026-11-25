//! Recent sessions, from what indexing already learned about each log, and
//! whether each one is running right now: Claude Code writes
//! `~/.claude/sessions/<pid>.json` for every live session. For a running one
//! we report its process and the processes above it (so a caller can tell
//! which terminal runs it), and the tmux pane it's in, if any.

use crate::index::{self, Meta};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Serialize, Debug)]
pub struct Session {
    #[serde(flatten)]
    pub meta: Meta,
    pub file: String,
    pub size: u64,
    pub live: Option<Live>,
}

#[derive(Serialize, Debug, Clone)]
pub struct Live {
    pub pid: u32,
    /// The processes above it, nearest first.
    pub ancestors: Vec<u32>,
    /// `session:window.pane` and the window's name, when it runs inside tmux.
    pub tmux: Option<TmuxPane>,
}

#[derive(Serialize, Debug, Clone)]
pub struct TmuxPane {
    pub pane: String,
    pub window: String,
}

#[derive(Serialize, Debug)]
pub struct List {
    pub sessions: Vec<Session>,
    pub total: usize,
}

pub struct Ask<'a> {
    pub index: &'a Path,
    pub claude_dir: &'a Path,
    pub limit: usize,
    /// Only sessions whose folder, title or first message contain this (any case).
    pub filter: Option<&'a str>,
}

pub fn list(ask: &Ask) -> Result<List> {
    let state = index::load_state(ask.index);
    let needle = ask
        .filter
        .map(str::to_lowercase)
        .filter(|n| !n.trim().is_empty());
    let mut all: Vec<(String, Meta)> = state
        .files
        .into_iter()
        .filter(|(_, f)| f.meta.messages > 0)
        .map(|(k, f)| (k, f.meta))
        .filter(|(_, m)| {
            needle.as_ref().is_none_or(|n| {
                [
                    Some(&m.project),
                    m.name.as_ref(),
                    m.title.as_ref(),
                    m.first.as_ref(),
                    Some(&m.session),
                ]
                .into_iter()
                .flatten()
                .any(|s| s.to_lowercase().contains(n.as_str()))
            })
        })
        .collect();
    all.sort_by_key(|(_, m)| std::cmp::Reverse(m.last_ts));
    // A session resumed from another folder gets a second log there: list it
    // once, by its most recent log.
    let mut seen = std::collections::HashSet::new();
    all.retain(|(_, m)| seen.insert(m.session.clone()));
    let total = all.len();
    all.truncate(ask.limit);

    let live = live_sessions(ask.claude_dir);
    let sessions = all
        .into_iter()
        .filter(|(k, _)| Path::new(k).exists())
        .map(|(file, meta)| {
            let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let live = live.get(&meta.session).cloned();
            Session {
                meta,
                file,
                size,
                live,
            }
        })
        .collect();
    Ok(List { sessions, total })
}

/// Live sessions by id: those whose `sessions/<pid>.json` names a process that
/// still exists.
fn live_sessions(claude_dir: &Path) -> HashMap<String, Live> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir(claude_dir.join("sessions")) else {
        return out;
    };
    let parents = process_parents();
    let panes = tmux_panes();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(pid) = p
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if !parents.contains_key(&pid) {
            continue; // a file left by a session that ended
        }
        let Some(id) = fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned)
            })
        else {
            continue;
        };
        let mut ancestors = Vec::new();
        let mut at = pid;
        while let Some(&up) = parents.get(&at) {
            if up <= 1 || ancestors.contains(&up) || ancestors.len() >= 16 {
                break;
            }
            ancestors.push(up);
            at = up;
        }
        let tmux = std::iter::once(pid)
            .chain(ancestors.iter().copied())
            .find_map(|p| panes.get(&p).cloned());
        out.insert(
            id,
            Live {
                pid,
                ancestors,
                tmux,
            },
        );
    }
    out
}

/// Every process's parent, from one `ps` call (the same on macOS and Linux).
fn process_parents() -> HashMap<u32, u32> {
    let Ok(o) = Command::new("ps")
        .args(["-e", "-o", "pid=", "-o", "ppid="])
        .output()
    else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// tmux panes by the pid of the program they started, when tmux is running.
fn tmux_panes() -> HashMap<u32, TmuxPane> {
    // Not a tab: tmux in the C locale (a plain ssh command) prints control
    // characters as `_`. Window names may hold spaces, so a word that won't.
    const SEP: &str = " ::encontra:: ";
    let fmt = format!("#{{pane_pid}}{SEP}#{{session_name}}:#{{window_index}}.#{{pane_index}}{SEP}#{{window_name}}");
    let Ok(o) = Command::new("tmux")
        .args(["list-panes", "-a", "-F", &fmt])
        .output()
    else {
        return HashMap::new();
    };
    if !o.status.success() {
        return HashMap::new();
    }
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, SEP);
            let pid = it.next()?.parse().ok()?;
            let pane = it.next()?.to_owned();
            let window = it.next().unwrap_or("").to_owned();
            Some((pid, TmuxPane { pane, window }))
        })
        .collect()
}

/// Names a session that isn't running, the way Claude Code's `/rename` does:
/// a `custom-title` line at the end of its log. A running session is refused:
/// its own process writes that log (rename it there, with `/rename`).
pub fn rename(file: &Path, session: &str, name: &str, claude_dir: &Path) -> Result<()> {
    use anyhow::bail;
    use std::io::{Read, Seek, SeekFrom, Write};
    let name = name.trim();
    if name.is_empty() {
        bail!("a name is needed");
    }
    if file.file_stem().and_then(|s| s.to_str()) != Some(session) {
        bail!("{} isn't the log of session {session}", file.display());
    }
    if live_sessions(claude_dir).contains_key(session) {
        bail!("session {session} is running: rename it there with /rename");
    }
    let mut f = fs::OpenOptions::new().read(true).append(true).open(file)?;
    // The new line starts on a line of its own.
    let len = f.metadata()?.len();
    let mut lead = "";
    if len > 0 {
        f.seek(SeekFrom::Start(len - 1))?;
        let mut last = [0u8; 1];
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            lead = "\n";
        }
    }
    let line =
        serde_json::json!({ "type": "custom-title", "customTitle": name, "sessionId": session });
    f.write_all(format!("{lead}{line}\n").as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_appends_what_claude_code_would() {
        let d = std::env::temp_dir().join(format!("encontra-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sessions")).unwrap();
        let log = d.join("abc.jsonl");
        fs::write(&log, "{\"type\":\"user\"}").unwrap(); // no newline at the end
        rename(&log, "abc", " Router work ", &d).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "custom-title");
        assert_eq!(last["customTitle"], "Router work");
        assert!(text.ends_with('\n'));
        assert!(rename(&log, "other", "x", &d).is_err());
        assert!(rename(&log, "abc", "  ", &d).is_err());
        // A running session (its process is this test) is left to Claude Code.
        fs::write(
            d.join("sessions")
                .join(format!("{}.json", std::process::id())),
            "{\"sessionId\":\"abc\"}",
        )
        .unwrap();
        assert!(rename(&log, "abc", "y", &d).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
