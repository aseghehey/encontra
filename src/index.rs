//! The index: one document per message piece, kept current incrementally.
//!
//! Session logs only grow, so each file's progress is its byte offset: an update
//! reads only what was appended since. A file that got shorter (rewritten) is
//! dropped from the index and read again from the start.

use crate::parse;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
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
/// Bumped whenever what's indexed or how changes: an index from another
/// version is thrown away and built again (old documents would otherwise stay).
const SCHEMA_VERSION: u32 = 6;
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
    /// Paths of attached files, found by name at a lower weight than the conversation.
    pub names: Field,
    /// The log line's id: the same message in a copy of the log is indexed once.
    pub uuid: Field,
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
        names: b.add_text_field("names", text),
        uuid: b.add_text_field("uuid", STRING),
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
        names: get("names")?,
        uuid: get("uuid")?,
    })
}

pub fn open(dir: &Path) -> Result<Index> {
    let index =
        Index::open_in_dir(dir).with_context(|| format!("no index at {}", dir.display()))?;
    index.tokenizers().register(TOKENIZER, analyzer());
    Ok(index)
}

/// Opens the index if it was built by this version, else starts a new one.
fn open_or_create(dir: &Path) -> Result<Index> {
    let current = load_state(dir).version == SCHEMA_VERSION && state_path(dir).exists();
    if dir.join("meta.json").exists() {
        if current {
            return open(dir);
        }
        discard(dir)?;
    }
    fs::create_dir_all(dir)?;
    let _ = fs::remove_file(state_path(dir));
    let (schema, _) = schema();
    let index = Index::create_in_dir(dir, schema)?;
    index.tokenizers().register(TOKENIZER, analyzer());
    Ok(index)
}

/// Removes an index this program built (and only that: a folder without an
/// index and our state file is left alone).
fn discard(dir: &Path) -> Result<()> {
    if !dir.join("meta.json").exists() {
        return Ok(());
    }
    eprintln!("herdr-search: rebuilding the index for format {SCHEMA_VERSION}");
    for e in fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.is_file() {
            fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct State {
    pub(crate) version: u32,
    pub(crate) files: BTreeMap<String, FileState>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub(crate) struct FileState {
    /// Bytes of the file already indexed (always at a line boundary).
    pub(crate) offset: u64,
    /// What the session is, gathered while indexing it (for listing sessions
    /// without reading their logs again).
    #[serde(default)]
    pub(crate) meta: Meta,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Meta {
    pub session: String,
    pub project: String,
    /// Claude Code's own title for it, the latest one.
    pub title: Option<String>,
    /// The name the person gave it (`/rename`), the latest one: shown over `title`.
    #[serde(default)]
    pub name: Option<String>,
    /// The person's first message, cut to `FIRST_CHARS`.
    pub first: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    /// Messages in it (the person's and the agent's).
    pub messages: u64,
}

const FIRST_CHARS: usize = 300;

impl Meta {
    fn add(&mut self, line: &parse::Line, session: &str, project: &str) {
        if self.session.is_empty() {
            self.session = session.to_owned();
        }
        // The folder the session started in: `claude --resume` finds a session
        // only from there. The log folder's name stands in until a line says.
        if self.project.is_empty() || (line.cwd.is_some() && !self.project.starts_with('/')) {
            self.project = project.to_owned();
        }
        if let Some(t) = &line.title {
            self.title = Some(t.clone());
        }
        if let Some(n) = &line.name {
            self.name = Some(n.clone());
        }
        if let Some(ts) = line.ts.filter(|&t| t > 0) {
            if self.first_ts == 0 || ts < self.first_ts {
                self.first_ts = ts;
            }
            self.last_ts = self.last_ts.max(ts);
        }
        for p in &line.pieces {
            match p.role {
                parse::Role::User | parse::Role::Assistant => self.messages += 1,
                parse::Role::Attachment => {}
            }
            if p.role == parse::Role::User && self.first.is_none() {
                let one_line: String = p.text.split_whitespace().collect::<Vec<_>>().join(" ");
                self.first = Some(one_line.chars().take(FIRST_CHARS).collect());
            }
        }
    }
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("herdr-search-state.json")
}

pub(crate) fn load_state(dir: &Path) -> State {
    fs::read(state_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<State>(&b).ok())
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
    meta: Meta,
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
        let known = state.files.get(&key).cloned().unwrap_or_default();
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
            meta: if rewritten {
                Meta::default()
            } else {
                known.meta
            },
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
    // Messages already indexed, by line id. Claude Code can leave a copy of a
    // session's log in several project folders; each message counts once.
    let indexed = Seen {
        before: indexed_ids(&index, &f)?,
        now: Mutex::new(HashSet::new()),
    };
    let docs = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let done: Mutex<Vec<(String, FileState)>> = Mutex::new(Vec::new());
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
                    match index_file(&writer, &f, t, &indexed) {
                        Ok((n, read, new_offset, meta)) => {
                            docs.fetch_add(n, Ordering::Relaxed);
                            bytes.fetch_add(read, Ordering::Relaxed);
                            done.lock().unwrap().push((
                                t.key.clone(),
                                FileState {
                                    offset: new_offset,
                                    meta,
                                },
                            ));
                        }
                        Err(e) => eprintln!("herdr-search: {}: {e:#}", t.path.display()),
                    }
                });
            }
        });
        start = end;
        if committed_at.elapsed().as_secs() >= 20 || start >= todo.len() {
            writer.commit()?;
            for (k, fs) in done.lock().unwrap().drain(..) {
                state.files.insert(k, fs);
            }
            save_state(&opts.index, &state)?;
            committed_at = Instant::now();
        }
    }
    writer.commit()?;
    for (k, fs) in done.lock().unwrap().drain(..) {
        state.files.insert(k, fs);
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
/// Line ids in the index when the update started, and those added since.
struct Seen {
    before: HashSet<String>,
    now: Mutex<HashSet<String>>,
}

impl Seen {
    /// Whether this line's messages still need indexing (and marks them done).
    /// A rewritten log's own old documents were just deleted, so only this
    /// run's ids count against it.
    fn first(&self, uuid: Option<&str>, rewritten: bool) -> bool {
        let Some(id) = uuid else { return true };
        if !rewritten && self.before.contains(id) {
            return false;
        }
        self.now.lock().unwrap().insert(id.to_owned())
    }
}

fn indexed_ids(index: &Index, f: &Fields) -> Result<HashSet<String>> {
    let searcher = index.reader()?.searcher();
    let mut out = HashSet::new();
    for seg in searcher.segment_readers() {
        let inv = seg.inverted_index(f.uuid)?;
        let mut terms = inv.terms().stream()?;
        while terms.advance() {
            if let Ok(s) = std::str::from_utf8(terms.key()) {
                out.insert(s.to_owned());
            }
        }
    }
    Ok(out)
}

fn index_file(
    writer: &IndexWriter,
    f: &Fields,
    t: &Todo,
    seen: &Seen,
) -> Result<(u64, u64, u64, Meta)> {
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
    let mut meta = t.meta.clone();
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
            meta.add(&line, session, project);
            if line.pieces.is_empty() || !seen.first(line.uuid.as_deref(), t.rewritten) {
                continue;
            }
            for p in &line.pieces {
                writer.add_document(doc!(
                    f.session => session,
                    f.file => t.key.as_str(),
                    f.project => project,
                    f.place => project,
                    f.offset => at,
                    f.ts => line.ts.unwrap_or(0),
                    f.role => p.role.as_str(),
                    f.body => if p.role == parse::Role::Attachment { "" } else { p.text.as_str() },
                    f.names => if p.role == parse::Role::Attachment { p.text.as_str() } else { "" },
                    f.uuid => line.uuid.as_deref().unwrap_or(""),
                ))?;
                docs += 1;
            }
        }
        offset += last_nl as u64 + 1;
    }
    Ok((docs, read_total, offset, meta))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("herdr-search-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("logs/-w-proj")).unwrap();
        d
    }

    fn prompt(text: &str) -> String {
        format!("{{\"type\":\"user\",\"sessionId\":\"s1\",\"cwd\":\"/w/proj\",\"timestamp\":\"2026-11-20T03:52:43Z\",\"message\":{{\"content\":\"{text}\"}}}}\n")
    }

    fn opts(d: &Path) -> Options {
        Options {
            root: d.join("logs"),
            index: d.join("index"),
            subagents: false,
            threads: 2,
            budget_secs: None,
        }
    }

    #[test]
    fn reads_each_byte_once_across_runs_and_rebuilds_on_a_new_format() {
        let d = scratch("incremental");
        let log = d.join("logs/-w-proj/s1.jsonl");
        fs::write(&log, prompt("first question") + &prompt("second question")).unwrap();

        let r1 = update(&opts(&d)).unwrap();
        assert_eq!((r1.changed, r1.docs), (1, 2));
        assert_eq!(r1.bytes_read, fs::metadata(&log).unwrap().len());

        // A second run (another start of the app, a reboot) reads nothing.
        let r2 = update(&opts(&d)).unwrap();
        assert_eq!((r2.changed, r2.docs, r2.bytes_read), (0, 0, 0));

        // A resumed session appends: only the new line is read.
        let added = prompt("third question");
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(added.as_bytes())
            .unwrap();
        let r3 = update(&opts(&d)).unwrap();
        assert_eq!((r3.docs, r3.bytes_read), (1, added.len() as u64));
        assert_eq!(stats(&d.join("index")).unwrap().docs, 3);

        // A half-written last line waits for the next run.
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"{\"type\":\"user\"")
            .unwrap();
        assert_eq!(update(&opts(&d)).unwrap().docs, 0);

        // An index from an older format is thrown away and built again, not added to.
        let state = state_path(&d.join("index"));
        let old = fs::read_to_string(&state).unwrap().replacen(
            &format!("\"version\":{SCHEMA_VERSION}"),
            "\"version\":1",
            1,
        );
        fs::write(&state, old).unwrap();
        let r4 = update(&opts(&d)).unwrap();
        assert_eq!(r4.docs, 3);
        assert_eq!(stats(&d.join("index")).unwrap().docs, 3);
        let _ = fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;

    #[test]
    fn a_copy_of_a_log_in_another_folder_is_indexed_once() {
        let d =
            std::env::temp_dir().join(format!("herdr-search-test-copies-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let line = |uuid: &str, text: &str| {
            format!("{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"sessionId\":\"s1\",\"cwd\":\"/w\",\"message\":{{\"content\":\"{text}\"}}}}\n")
        };
        for dir in ["-w-a", "-w-b"] {
            fs::create_dir_all(d.join("logs").join(dir)).unwrap();
            fs::write(
                d.join("logs").join(dir).join("s1.jsonl"),
                line("u1", "same question"),
            )
            .unwrap();
        }
        let opts = Options {
            root: d.join("logs"),
            index: d.join("index"),
            subagents: false,
            threads: 2,
            budget_secs: None,
        };
        let r = update(&opts).unwrap();
        assert_eq!((r.changed, r.docs), (2, 1));
        // The copy goes on with its own messages: those are new.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(d.join("logs/-w-b/s1.jsonl"))
            .unwrap();
        std::io::Write::write_all(&mut f, line("u2", "a new one").as_bytes()).unwrap();
        assert_eq!(update(&opts).unwrap().docs, 1);
        assert_eq!(stats(&d.join("index")).unwrap().docs, 2);
        let _ = fs::remove_dir_all(&d);
    }
}
