//! The index: one document per message piece, kept current incrementally.
//!
//! Session logs only grow, so each file's progress is its byte offset: an update
//! reads only what was appended since. A file that got shorter (rewritten) is
//! dropped from the index and read again from the start.

use crate::parse::{self, Role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
};
use tantivy::{doc, Index, IndexWriter, Term};

pub const TOKENIZER: &str = "conv";
const SCHEMA_VERSION: u32 = 1;
/// Bytes read from one file per pass, so one huge log can't hold the rest back.
const CHUNK: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Fields {
    pub session: Field,
    pub file: Field,
    pub project: Field,
    /// The project folder's words, for `in:` filters on any part of its path.
    pub place: Field,
    pub offset: Field,
    pub ts: Field,
    pub role: Field,
    pub body: Field,
    pub tools: Field,
}

pub fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let f = Fields {
        session: b.add_text_field("session", STRING | STORED),
        file: b.add_text_field("file", STRING | STORED),
        project: b.add_text_field("project", STRING | STORED),
        place: b.add_text_field(
            "place",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(TOKENIZER)
                    .set_index_option(IndexRecordOption::Basic),
            ),
        ),
        offset: b.add_u64_field("offset", STORED | FAST),
        ts: b.add_i64_field("ts", INDEXED | STORED | FAST),
        role: b.add_text_field("role", STRING | STORED),
        body: b.add_text_field("body", text.clone()),
        tools: b.add_text_field("tools", text),
    };
    (b.build(), f)
}

/// Words split on anything that isn't a letter or digit, lowercased and with
/// accents folded, no stemming: `ECONNREFUSED`, `src/main.rs` and `café` all
/// match what was typed.
pub fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(64))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

pub fn fields_of(index: &Index) -> Result<Fields> {
    let s = index.schema();
    let get = |n: &str| {
        s.get_field(n)
            .with_context(|| format!("index has no {n} field"))
    };
    Ok(Fields {
        session: get("session")?,
        file: get("file")?,
        project: get("project")?,
        place: get("place")?,
        offset: get("offset")?,
        ts: get("ts")?,
        role: get("role")?,
        body: get("body")?,
        tools: get("tools")?,
    })
}

pub fn open(dir: &Path) -> Result<Index> {
    let index =
        Index::open_in_dir(dir).with_context(|| format!("no index at {}", dir.display()))?;
    index.tokenizers().register(TOKENIZER, analyzer());
    Ok(index)
}

fn open_or_create(dir: &Path) -> Result<Index> {
    if dir.join("meta.json").exists() {
        return open(dir);
    }
    fs::create_dir_all(dir)?;
    let (schema, _) = schema();
    let index = Index::create_in_dir(dir, schema)?;
    index.tokenizers().register(TOKENIZER, analyzer());
    Ok(index)
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    version: u32,
    files: BTreeMap<String, FileState>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
struct FileState {
    /// Bytes of the file already indexed (always at a line boundary).
    offset: u64,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("herdr-search-state.json")
}

fn load_state(dir: &Path) -> State {
    fs::read(state_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<State>(&b).ok())
        .filter(|s| s.version == SCHEMA_VERSION)
        .unwrap_or(State {
            version: SCHEMA_VERSION,
            ..Default::default()
        })
}

fn save_state(dir: &Path, state: &State) -> Result<()> {
    let tmp = dir.join("herdr-search-state.json.tmp");
    fs::write(&tmp, serde_json::to_vec(state)?)?;
    fs::rename(&tmp, state_path(dir))?;
    Ok(())
}

pub struct Options {
    pub root: PathBuf,
    pub index: PathBuf,
    pub subagents: bool,
    pub threads: usize,
    /// Stop starting new files after this long; the rest wait for the next run.
    pub budget_secs: Option<u64>,
}

#[derive(Serialize, Default)]
pub struct Report {
    pub files: usize,
    pub changed: usize,
    pub remaining: usize,
    pub docs: u64,
    pub bytes_read: u64,
    pub secs: f64,
}

struct Todo {
    path: PathBuf,
    key: String,
    from: u64,
    size: u64,
    rewritten: bool,
}

/// Session logs under `root`, newest first. Sub-agent logs live in
/// `<project>/<session>/subagents/` and are left out unless asked for.
fn session_logs(root: &Path, subagents: bool) -> Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                let depth = p
                    .strip_prefix(root)
                    .map(|r| r.components().count())
                    .unwrap_or(0);
                // <project> is depth 1; <project>/<session>/subagents only when asked.
                if depth == 1 || (subagents && depth <= 3) {
                    stack.push(p);
                }
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(m) = e.metadata() {
                    out.push((p, m.len(), m.modified().unwrap_or(UNIX_EPOCH)));
                }
            }
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.2));
    Ok(out)
}

pub fn update(opts: &Options) -> Result<Report> {
    let started = Instant::now();
    let index = open_or_create(&opts.index)?;
    let f = fields_of(&index)?;
    let mut state = load_state(&opts.index);
    let logs = session_logs(&opts.root, opts.subagents)?;

    let mut todo = Vec::new();
    for (path, size, _) in &logs {
        let key = path.to_string_lossy().into_owned();
        let known = state.files.get(&key).copied().unwrap_or_default();
        if size == &known.offset {
            continue;
        }
        let rewritten = *size < known.offset;
        todo.push(Todo {
            path: path.clone(),
            key,
            from: if rewritten { 0 } else { known.offset },
            size: *size,
            rewritten,
        });
    }

    let mut report = Report {
        files: logs.len(),
        ..Default::default()
    };
    if todo.is_empty() {
        report.secs = started.elapsed().as_secs_f64();
        return Ok(report);
    }

    // A second writer (another update already running) fails here: that's fine,
    // the running one will catch up.
    let mut writer: IndexWriter = index
        .writer_with_num_threads(opts.threads.clamp(1, 8), 512 * 1024 * 1024)
        .context("another update holds the index")?;
    let docs = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let done: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
    let next = AtomicU64::new(0);
    let deadline = opts
        .budget_secs
        .map(|s| started + std::time::Duration::from_secs(s));
    let mut committed_at = Instant::now();

    // Work through the files in batches; commit (and save progress) after each,
    // so an interrupted first build keeps what it did.
    let batch = 256usize;
    let mut start = 0usize;
    while start < todo.len() {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let end = (start + batch).min(todo.len());
        let slice = &todo[start..end];
        next.store(0, Ordering::SeqCst);
        std::thread::scope(|scope| {
            for _ in 0..opts.threads.max(1) {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst) as usize;
                    let Some(t) = slice.get(i) else { break };
                    if t.rewritten {
                        writer.delete_term(Term::from_field_text(f.file, &t.key));
                    }
                    match index_file(&writer, &f, t) {
                        Ok((n, read, new_offset)) => {
                            docs.fetch_add(n, Ordering::Relaxed);
                            bytes.fetch_add(read, Ordering::Relaxed);
                            done.lock().unwrap().push((t.key.clone(), new_offset));
                        }
                        Err(e) => eprintln!("herdr-search: {}: {e:#}", t.path.display()),
                    }
                });
            }
        });
        start = end;
        if committed_at.elapsed().as_secs() >= 20 || start >= todo.len() {
            writer.commit()?;
            for (k, off) in done.lock().unwrap().drain(..) {
                state.files.insert(k, FileState { offset: off });
            }
            save_state(&opts.index, &state)?;
            committed_at = Instant::now();
        }
    }
    writer.commit()?;
    for (k, off) in done.lock().unwrap().drain(..) {
        state.files.insert(k, FileState { offset: off });
    }
    // Forget logs that are gone (their documents stay until a rebuild; search
    // drops hits whose file no longer exists).
    let live: std::collections::HashSet<String> = logs
        .iter()
        .map(|l| l.0.to_string_lossy().into_owned())
        .collect();
    state.files.retain(|k, _| live.contains(k));
    save_state(&opts.index, &state)?;
    writer.wait_merging_threads()?;

    report.changed = start.min(todo.len());
    report.remaining = todo.len() - report.changed;
    report.docs = docs.load(Ordering::Relaxed);
    report.bytes_read = bytes.load(Ordering::Relaxed);
    report.secs = started.elapsed().as_secs_f64();
    Ok(report)
}

/// Indexes the complete lines of `t.path` from `t.from`. Returns the documents
/// added, the bytes read and the new offset (just past the last full line).
fn index_file(writer: &IndexWriter, f: &Fields, t: &Todo) -> Result<(u64, u64, u64)> {
    let mut file = fs::File::open(&t.path)?;
    let session_from_name = t
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project_from_dir = t
        .path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut offset = t.from;
    let mut docs = 0u64;
    let mut read_total = 0u64;
    let mut buf = Vec::new();
    while offset < t.size {
        let want = (t.size - offset).min(CHUNK);
        file.seek(SeekFrom::Start(offset))?;
        buf.clear();
        (&mut file).take(want).read_to_end(&mut buf)?;
        read_total += buf.len() as u64;
        // Only complete lines; a partial last line waits for the next update.
        let Some(last_nl) = buf.iter().rposition(|&c| c == b'\n') else {
            if buf.len() as u64 >= CHUNK {
                // One line longer than a chunk: skip past it rather than stall
                // (its tail fails to parse and is skipped too).
                offset += buf.len() as u64;
                continue;
            }
            break;
        };
        let mut line_start = 0usize;
        for (i, &c) in buf[..=last_nl].iter().enumerate() {
            if c != b'\n' {
                continue;
            }
            let raw = &buf[line_start..i];
            let at = offset + line_start as u64;
            line_start = i + 1;
            let Ok(raw) = std::str::from_utf8(raw) else {
                continue;
            };
            let Some(line) = parse::parse_line(raw) else {
                continue;
            };
            let session = line.session.as_deref().unwrap_or(&session_from_name);
            let project = line.cwd.as_deref().unwrap_or(&project_from_dir);
            for p in line.pieces {
                let (body, tools) = match p.role {
                    Role::Tool => ("", p.text.as_str()),
                    _ => (p.text.as_str(), ""),
                };
                writer.add_document(doc!(
                    f.session => session,
                    f.file => t.key.as_str(),
                    f.project => project,
                    f.place => project,
                    f.offset => at,
                    f.ts => line.ts.unwrap_or(0),
                    f.role => p.role.as_str(),
                    f.body => body,
                    f.tools => tools,
                ))?;
                docs += 1;
            }
        }
        offset += last_nl as u64 + 1;
    }
    Ok((docs, read_total, offset))
}

#[derive(Serialize)]
pub struct Stats {
    pub docs: u64,
    pub segments: usize,
    pub files: usize,
    pub bytes_on_disk: u64,
}

pub fn stats(dir: &Path) -> Result<Stats> {
    let index = open(dir)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut bytes = 0;
    for e in fs::read_dir(dir)?.flatten() {
        bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
    }
    Ok(Stats {
        docs: searcher.num_docs(),
        segments: searcher.segment_readers().len(),
        files: load_state(dir).files.len(),
        bytes_on_disk: bytes,
    })
}
