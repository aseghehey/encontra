//! herdr-search: full-text search over Claude Code session logs, for herdr-chat.
//!
//!   herdr-search index [--root DIR] [--index DIR] [--subagents] [--threads N] [--budget SECS]
//!   herdr-search query [--index DIR] [--sessions N] [--hits N] [--pool N] [--in-order N] <query>
//!   herdr-search stats [--index DIR]
//!
//! Every command prints one JSON object on stdout.

mod index;
mod parse;
mod query;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
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
}

fn print(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(v)?);
    Ok(())
}

fn run() -> Result<()> {
    let mut all: Vec<String> = std::env::args().skip(1).collect();
    if all.is_empty() {
        bail!("usage: herdr-search index|query|stats [options]");
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
                root: a
                    .value("--root")?
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home().join(".claude/projects")),
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
            let q = a.rest.join(" ");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            print(&query::search(
                &index_dir,
                &query::Ask {
                    q: &q,
                    now,
                    sessions,
                    per_session,
                    in_order,
                    pool,
                },
            )?)
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
