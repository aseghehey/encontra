//! herdr-search: full-text search over Claude Code session logs, for herdr-chat.
//!
//!   herdr-search index [--root DIR] [--index DIR] [--subagents] [--threads N] [--budget SECS]
//!   herdr-search query [--index DIR] [--sessions N] [--hits N] [--pool N] [--in-order N] [--update] <query>
//!   herdr-search sessions [--index DIR] [--limit N] [--filter TEXT] [--update]
//!   herdr-search page --file LOG [--from OFFSET] [--to OFFSET]
//!   herdr-search rename --file LOG --session ID --name NAME   (a session that isn't running)
//!   herdr-search stats [--index DIR]
//!
//! Every command prints one JSON object on stdout. `query` in a terminal prints
//! readable results instead (after bringing the index up to date); `--json`
//! keeps JSON there too. `--update` brings the index up to date first.

mod index;
mod page;
mod parse;
mod query;
mod sessions;
mod text;

use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_index() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    data.join("herdr-search/index")
}

struct Args {
    rest: Vec<String>,
}

impl Args {
    /// Removes `--name VALUE` and returns the value.
    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let Some(i) = self.rest.iter().position(|a| a == name) else {
            return Ok(None);
        };
        if i + 1 >= self.rest.len() {
            bail!("{name} needs a value");
        }
        let v = self.rest.remove(i + 1);
        self.rest.remove(i);
        Ok(Some(v))
    }

    fn flag(&mut self, name: &str) -> bool {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }

    fn num(&mut self, name: &str, default: usize) -> Result<usize> {
        Ok(match self.value(name)? {
            Some(v) => v
                .parse()
                .with_context(|| format!("{name} wants a number"))?,
            None => default,
        })
    }

    fn offset(&mut self, name: &str) -> Result<Option<u64>> {
        self.value(name)?
            .map(|v| v.parse())
            .transpose()
            .with_context(|| format!("{name} wants a byte offset"))
    }

    fn root(&mut self) -> Result<PathBuf> {
        Ok(self
            .value("--root")?
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".claude/projects")))
    }
}

fn print(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(v)?);
    Ok(())
}

/// Brings the index up to date, at most 20 seconds of it; a failure only warns.
fn update_quietly(root: &Path, index_dir: &Path) {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let opts = index::Options {
        root: root.to_path_buf(),
        index: index_dir.to_path_buf(),
        subagents: false,
        threads,
        budget_secs: Some(20),
    };
    if let Err(e) = index::update(&opts) {
        eprintln!("herdr-search: index not updated: {e:#}");
    }
}

fn run() -> Result<()> {
    let mut all: Vec<String> = std::env::args().skip(1).collect();
    if all.is_empty() {
        bail!("usage: herdr-search index|query|sessions|page|rename|stats [options]");
    }
    let cmd = all.remove(0);
    let mut a = Args { rest: all };
    let index_dir = a
        .value("--index")?
        .map(PathBuf::from)
        .unwrap_or_else(default_index);
    match cmd.as_str() {
        "index" => {
            let opts = index::Options {
                root: a.root()?,
                index: index_dir,
                subagents: a.flag("--subagents"),
                threads: a.num(
                    "--threads",
                    std::thread::available_parallelism()
                        .map_or(4, |n| n.get())
                        .min(16),
                )?,
                budget_secs: a
                    .value("--budget")?
                    .map(|v| v.parse())
                    .transpose()
                    .context("--budget wants seconds")?,
            };
            print(&index::update(&opts)?)
        }
        "query" => {
            let sessions = a.num("--sessions", 20)?;
            let per_session = a.num("--hits", 3)?;
            let pool = a.num("--pool", query::POOL)?;
            let in_order = a
                .value("--in-order")?
                .map(|v| v.parse())
                .transpose()
                .context("--in-order wants a number")?;
            let json = a.flag("--json") || !std::io::stdout().is_terminal();
            let root = a.root()?;
            // Someone at a terminal wants what was said a minute ago too.
            if a.flag("--update") || !json {
                update_quietly(&root, &index_dir);
            }
            let q = a.rest.join(" ");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            let answer = query::search(
                &index_dir,
                &query::Ask {
                    q: &q,
                    now,
                    sessions,
                    per_session,
                    in_order,
                    pool,
                },
            )?;
            if json {
                print(&answer)
            } else {
                text::print(&answer);
                Ok(())
            }
        }
        "sessions" => {
            let root = a.root()?;
            if a.flag("--update") {
                update_quietly(&root, &index_dir);
            }
            let limit = a.num("--limit", 200)?;
            let filter = a.value("--filter")?;
            let claude_dir = root
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".claude"));
            print(&sessions::list(&sessions::Ask {
                index: &index_dir,
                claude_dir: &claude_dir,
                limit,
                filter: filter.as_deref(),
            })?)
        }
        "page" => {
            let file = a.value("--file")?.context("page needs --file")?;
            let from = a.offset("--from")?.unwrap_or(0);
            let to = a.offset("--to")?;
            print(&page::page(Path::new(&file), from, to)?)
        }
        "rename" => {
            let file = a.value("--file")?.context("rename needs --file")?;
            let session = a.value("--session")?.context("rename needs --session")?;
            let name = a.value("--name")?.context("rename needs --name")?;
            let root = a.root()?;
            let claude_dir = root
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".claude"));
            sessions::rename(Path::new(&file), &session, &name, &claude_dir)?;
            update_quietly(&root, &index_dir);
            print(&serde_json::json!({ "ok": true }))
        }
        "stats" => print(&index::stats(&index_dir)?),
        other => bail!("unknown command {other}"),
    }
}

fn main() {
    if let Err(e) = run() {
        println!("{}", serde_json::json!({ "error": format!("{e:#}") }));
        std::process::exit(1);
    }
}
