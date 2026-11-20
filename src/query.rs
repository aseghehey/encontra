//! Searching: what the person typed into a Tantivy query, and hits grouped by session.
//!
//! Plain words must all match (the last one may be a prefix, since it's the one
//! still being typed); `"quoted words"` match as a phrase; `-word` excludes.
//! Conversation comes first; files the person attached are found by name, at a
//! lower weight. Filters: `in:<part of the project path>`, `from:me|agent|file`,
//! `after:`/`before:` (`2026-11-01` or `7d`), `session:<id>`. A filter whose
//! value can't be understood refuses the whole search instead of being ignored.
//! A query that finds nothing is tried again with typo tolerance.
//!
//! The query is built from terms, never handed to a query parser, so nothing
//! the person types is a syntax error.

use crate::index::{self, Fields};
use crate::parse;
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, TantivyDocument, Term};

/// How much a match in an attached file's name counts next to one in the conversation.
const ATTACHMENT_WEIGHT: f32 = 0.4;
/// Hits looked at before grouping by session, unless the caller says otherwise.
pub const POOL: usize = 3000;
const SNIPPET_CHARS: usize = 220;

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    /// Free words in order; `prefix` marks the last one when no space follows it.
    pub words: Vec<String>,
    pub prefix: bool,
    pub phrases: Vec<String>,
    pub exclude: Vec<String>,
    pub places: Vec<String>,
    pub role: Option<&'static str>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub session: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Refused(pub String);

/// Splits the query into words, quoted phrases and filters. `now` (Unix ms)
/// anchors relative dates.
pub fn parse_query(q: &str, now: i64) -> Result<Parsed, Refused> {
    let mut p = Parsed::default();
    let mut rest = q;
    while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
        rest = &rest[start..];
        if let Some(inner) = rest.strip_prefix('"') {
            let end = inner.find('"').unwrap_or(inner.len());
            let phrase = inner[..end].trim();
            if !phrase.is_empty() {
                p.phrases.push(phrase.to_owned());
            }
            rest = inner.get(end + 1..).unwrap_or("");
            p.prefix = false;
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        rest = &rest[end..];
        let last = rest.is_empty();
        if let Some((key, value)) = tok.split_once(':') {
            let handled = match key {
                "in" => {
                    if value.is_empty() {
                        return Err(Refused(tok.into()));
                    }
                    p.places.push(value.to_owned());
                    true
                }
                "from" => {
                    p.role = Some(match value {
                        "me" | "user" | "you" => "user",
                        "agent" | "claude" | "assistant" | "ai" => "assistant",
                        "file" | "files" | "attachment" | "attachments" => "attachment",
                        _ => return Err(Refused(tok.into())),
                    });
                    true
                }
                "after" | "before" => {
                    let t = parse_when(value, now).ok_or_else(|| Refused(tok.into()))?;
                    if key == "after" {
                        p.after = Some(t);
                    } else {
                        p.before = Some(t);
                    }
                    true
                }
                "session" => {
                    if value.is_empty() {
                        return Err(Refused(tok.into()));
                    }
                    p.session = Some(value.to_owned());
                    true
                }
                // Anything else with a colon (a URL, `std::fs`) is just text.
                _ => false,
            };
            if handled {
                p.prefix = false;
                continue;
            }
        }
        if let Some(word) = tok.strip_prefix('-').filter(|w| !w.is_empty()) {
            p.exclude.push(word.to_owned());
            p.prefix = false;
            continue;
        }
        p.words.push(tok.to_owned());
        p.prefix = last;
    }
    Ok(p)
}

/// `2026-11-01` (start of that day, UTC) or `7d` / `12h` / `2w` before `now`.
fn parse_when(v: &str, now: i64) -> Option<i64> {
    if let Some(n) = v.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return Some(now - n * 86_400_000);
    }
    if let Some(n) = v.strip_suffix('h').and_then(|n| n.parse::<i64>().ok()) {
        return Some(now - n * 3_600_000);
    }
    if let Some(n) = v.strip_suffix('w').and_then(|n| n.parse::<i64>().ok()) {
        return Some(now - n * 7 * 86_400_000);
    }
    if v.len() == 10 {
        let t = parse::parse_iso_millis(&format!("{v}T00:00:00Z"))?;
        // Reject dates that don't exist (2026-02-30 would roll into March).
        let back = format!("{v}T00:00:00Z");
        let (y, m, d) = (&back[0..4], &back[5..7], &back[8..10]);
        let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
        let y: i64 = y.parse().ok()?;
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let days = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if m == 0 || m > 12 || d == 0 || d > days[(m - 1) as usize] {
            return None;
        }
        return Some(t);
    }
    None
}

/// The index's words for `text`, as the index tokenized them.
fn tokens(text: &str) -> Vec<String> {
    let mut a = index::analyzer();
    let mut ts = a.token_stream(text);
    let mut out = Vec::new();
    while ts.advance() {
        out.push(ts.token().text.clone());
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Exact,
    Fuzzy,
}

fn term_in(
    field: tantivy::schema::Field,
    word: &str,
    last_prefix: bool,
    mode: Mode,
) -> Box<dyn Query> {
    let term = Term::from_field_text(field, word);
    let n = word.chars().count();
    match mode {
        Mode::Fuzzy if n >= 4 => {
            let d = if n >= 8 { 2 } else { 1 };
            if last_prefix {
                Box::new(FuzzyTermQuery::new_prefix(term, d, true))
            } else {
                Box::new(FuzzyTermQuery::new(term, d, true))
            }
        }
        _ if last_prefix && n >= 2 => Box::new(BooleanQuery::new(vec![
            (
                Occur::Should,
                Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs))
                    as Box<dyn Query>,
            ),
            (
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(FuzzyTermQuery::new_prefix(term, 0, false)),
                    0.5,
                )),
            ),
        ])),
        _ => Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)),
    }
}

/// Words that tokenize to several index words (`src/main.rs`) match as a phrase.
fn words_query(
    field_weight: &[(tantivy::schema::Field, f32)],
    toks: &[String],
    last_prefix: bool,
    mode: Mode,
) -> Box<dyn Query> {
    let mut either: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for &(field, w) in field_weight {
        let q: Box<dyn Query> = if toks.len() == 1 {
            term_in(field, &toks[0], last_prefix, mode)
        } else {
            Box::new(PhraseQuery::new(
                toks.iter()
                    .map(|t| Term::from_field_text(field, t))
                    .collect(),
            ))
        };
        either.push((
            Occur::Should,
            if w == 1.0 {
                q
            } else {
                Box::new(BoostQuery::new(q, w))
            },
        ));
    }
    Box::new(BooleanQuery::new(either))
}

pub fn build(f: &Fields, p: &Parsed, mode: Mode) -> Option<Box<dyn Query>> {
    let text_fields = [(f.body, 1.0), (f.names, ATTACHMENT_WEIGHT)];
    let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut any_text = false;
    let mut all_words: Vec<String> = Vec::new();
    let n = p.words.len();
    for (i, w) in p.words.iter().enumerate() {
        let toks = tokens(w);
        if toks.is_empty() {
            continue;
        }
        let last_prefix = p.prefix && i + 1 == n && toks.len() == 1;
        must.push((
            Occur::Must,
            words_query(&text_fields, &toks, last_prefix, mode),
        ));
        all_words.extend(toks);
        any_text = true;
    }
    for ph in &p.phrases {
        let toks = tokens(ph);
        if toks.is_empty() {
            continue;
        }
        must.push((
            Occur::Must,
            words_query(&text_fields, &toks, false, Mode::Exact),
        ));
        any_text = true;
    }
    // Words next to each other, in order, rank above the same words apart.
    if all_words.len() >= 2 {
        let near: Vec<Term> = all_words
            .iter()
            .map(|t| Term::from_field_text(f.body, t))
            .collect();
        must.push((
            Occur::Should,
            Box::new(BoostQuery::new(
                Box::new(PhraseQuery::new_with_offset_and_slop(
                    near.into_iter().enumerate().collect(),
                    3,
                )),
                2.0,
            )),
        ));
    }
    for w in &p.exclude {
        for t in tokens(w) {
            for field in [f.body, f.names] {
                must.push((
                    Occur::MustNot,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, &t),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
    }
    for place in &p.places {
        for t in tokens(place) {
            must.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.place, &t),
                    IndexRecordOption::Basic,
                )),
            ));
        }
    }
    if let Some(role) = p.role {
        must.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(f.role, role),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if let Some(s) = &p.session {
        must.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(f.session, s),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if p.after.is_some() || p.before.is_some() {
        let lo = p.after.map_or(Bound::Unbounded, |t| {
            Bound::Included(Term::from_field_i64(f.ts, t))
        });
        let hi = p.before.map_or(Bound::Unbounded, |t| {
            Bound::Excluded(Term::from_field_i64(f.ts, t))
        });
        must.push((Occur::Must, Box::new(RangeQuery::new(lo, hi))));
    }
    if !any_text {
        // Only filters (`from:me session:<id>`: every prompt in a session):
        // everything they let through. Nothing at all asks for nothing.
        let filtered = p.role.is_some()
            || p.session.is_some()
            || !p.places.is_empty()
            || p.after.is_some()
            || p.before.is_some();
        if !filtered {
            return None;
        }
        must.push((Occur::Must, Box::new(tantivy::query::AllQuery)));
    }
    Some(Box::new(BooleanQuery::new(must)))
}

#[derive(Serialize, Debug)]
pub struct Hit {
    /// The log the offset is in: a session resumed from another folder has two.
    pub file: String,
    pub offset: u64,
    pub ts: i64,
    pub role: String,
    pub snippet: String,
    /// `[start, end)` in UTF-16 code units of `snippet` (JavaScript string offsets).
    pub highlights: Vec<[usize; 2]>,
    pub score: f32,
}

#[derive(Serialize, Debug)]
pub struct SessionHits {
    pub session: String,
    pub project: String,
    pub file: String,
    pub best: f32,
    pub last_ts: i64,
    pub matches: usize,
    pub hits: Vec<Hit>,
}

#[derive(Serialize, Debug)]
pub struct Answer {
    pub query: String,
    pub mode: Option<Mode>,
    pub total: usize,
    pub refused: Option<String>,
    pub sessions: Vec<SessionHits>,
}

pub struct Ask<'a> {
    pub q: &'a str,
    pub now: i64,
    pub sessions: usize,
    pub per_session: usize,
    /// For finding inside one session: every hit (up to this many), in log order.
    pub in_order: Option<usize>,
    /// Hits looked at before grouping (more finds more sessions, a little slower).
    pub pool: usize,
    /// Newest matches first (sessions by their newest match), instead of best first.
    pub recent: bool,
}

pub fn search(index_dir: &Path, ask: &Ask) -> Result<Answer> {
    let index = index::open(index_dir)?;
    search_index(&index, ask, true)
}

pub fn search_index(index: &Index, ask: &Ask, check_files: bool) -> Result<Answer> {
    let f = index::fields_of(index)?;
    let mut answer = Answer {
        query: ask.q.to_owned(),
        mode: None,
        total: 0,
        refused: None,
        sessions: vec![],
    };
    let p = match parse_query(ask.q, ask.now) {
        Ok(p) => p,
        Err(Refused(tok)) => {
            answer.refused = Some(tok);
            return Ok(answer);
        }
    };
    let searcher = index.reader()?.searcher();
    let pool = ask.in_order.unwrap_or(ask.pool).max(1);
    let mut found = Vec::new();
    for mode in [Mode::Exact, Mode::Fuzzy] {
        let Some(q) = build(&f, &p, mode) else {
            return Ok(answer);
        };
        let (top, count): (Vec<(f32, tantivy::DocAddress)>, usize) = if ask.recent {
            let (by_time, count) = searcher.search(
                &q,
                &(
                    TopDocs::with_limit(pool)
                        .order_by_fast_field::<i64>("ts", tantivy::Order::Desc),
                    Count,
                ),
            )?;
            (by_time.into_iter().map(|(_, a)| (0.0, a)).collect(), count)
        } else {
            searcher.search(&q, &(TopDocs::with_limit(pool).order_by_score(), Count))?
        };
        if count > 0 || !p.phrases.is_empty() || mode == Mode::Fuzzy {
            answer.mode = Some(mode);
            answer.total = count;
            found = top;
            break;
        }
    }
    let marks = Marks::new(&p, answer.mode == Some(Mode::Fuzzy));
    let mut exists: HashMap<String, bool> = HashMap::new();
    let mut groups: Vec<SessionHits> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (score, addr) in found {
        let d: TantivyDocument = searcher.doc(addr)?;
        let s = |field| {
            d.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        };
        let file = s(f.file);
        if check_files
            && !*exists
                .entry(file.clone())
                .or_insert_with(|| Path::new(&file).exists())
        {
            continue;
        }
        let session = s(f.session);
        let ts = d.get_first(f.ts).and_then(|v| v.as_i64()).unwrap_or(0);
        let offset = d.get_first(f.offset).and_then(|v| v.as_u64()).unwrap_or(0);
        let role = s(f.role);
        let text = if role == "attachment" {
            s(f.names)
        } else {
            s(f.body)
        };
        let (snippet, highlights) = marks.snippet(&text);
        let hit = Hit {
            file: file.clone(),
            offset,
            ts,
            role,
            snippet,
            highlights,
            score,
        };
        let g = match at.get(&session) {
            Some(&i) => &mut groups[i],
            None => {
                at.insert(session.clone(), groups.len());
                groups.push(SessionHits {
                    session,
                    project: s(f.project),
                    file,
                    best: score,
                    last_ts: ts,
                    matches: 0,
                    hits: vec![],
                });
                groups.last_mut().unwrap()
            }
        };
        g.matches += 1;
        g.last_ts = g.last_ts.max(ts);
        let keep = if ask.in_order.is_some() {
            usize::MAX
        } else {
            ask.per_session
        };
        if g.hits.len() < keep {
            g.hits.push(hit);
        }
    }
    if ask.in_order.is_some() {
        for g in &mut groups {
            g.hits.sort_by_key(|h| h.offset);
        }
    }
    groups.truncate(ask.sessions);
    answer.sessions = groups;
    Ok(answer)
}

/// Which words of a stored text to highlight, and the snippet around the first.
struct Marks {
    exact: HashSet<String>,
    prefix: Option<String>,
    fuzzy: Vec<String>,
}

impl Marks {
    fn new(p: &Parsed, fuzzy: bool) -> Marks {
        let mut exact = HashSet::new();
        let mut prefix = None;
        let mut fz = Vec::new();
        let n = p.words.len();
        for (i, w) in p.words.iter().enumerate() {
            let toks = tokens(w);
            let last = p.prefix && i + 1 == n && toks.len() == 1;
            for t in toks {
                if fuzzy && t.chars().count() >= 4 {
                    fz.push(t);
                } else if last {
                    prefix = Some(t);
                } else {
                    exact.insert(t);
                }
            }
        }
        for ph in &p.phrases {
            exact.extend(tokens(ph));
        }
        Marks {
            exact,
            prefix,
            fuzzy: fz,
        }
    }

    fn hit(&self, tok: &str) -> bool {
        self.exact.contains(tok)
            || self.prefix.as_deref().is_some_and(|p| tok.starts_with(p))
            || self.fuzzy.iter().any(|w| {
                let d = if w.chars().count() >= 8 { 2 } else { 1 };
                edit_distance(w, tok) <= d
                    || (tok.len() > w.len()
                        && edit_distance(
                            w,
                            &tok[..tok
                                .char_indices()
                                .nth(w.chars().count())
                                .map_or(tok.len(), |c| c.0)],
                        ) <= d)
            })
    }

    /// About `SNIPPET_CHARS` characters around the first highlighted word, on
    /// one line where possible, with every highlighted word inside it marked.
    fn snippet(&self, text: &str) -> (String, Vec<[usize; 2]>) {
        let mut a = index::analyzer();
        let mut ts = a.token_stream(text);
        let mut spans = Vec::new();
        while ts.advance() {
            let t = ts.token();
            if self.hit(&t.text) {
                spans.push((t.offset_from, t.offset_to));
            }
        }
        let first = spans.first().map_or(0, |s| s.0);
        let (from, to) = window(text, first, SNIPPET_CHARS);
        let piece = &text[from..to];
        let lead = if from > 0 { "…" } else { "" };
        let tail = if to < text.len() { "…" } else { "" };
        let mut out = String::with_capacity(piece.len() + 8);
        out.push_str(lead);
        let base16 = lead.encode_utf16().count();
        let mut highlights = Vec::new();
        for (s, e) in spans {
            if s < from || e > to {
                continue;
            }
            let s16 = base16 + text[from..s].encode_utf16().count();
            let e16 = s16 + text[s..e].encode_utf16().count();
            highlights.push([s16, e16]);
        }
        // Newlines and runs of spaces as single spaces, keeping the offsets
        // above valid: each is replaced one character for one.
        out.extend(piece.chars().map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        }));
        out.push_str(tail);
        (out, highlights)
    }
}

/// A byte window of about `chars` characters that holds `at`, starting a little before it.
fn window(text: &str, at: usize, chars: usize) -> (usize, usize) {
    let from = text[..at]
        .char_indices()
        .rev()
        .take(chars / 3)
        .last()
        .map_or(at, |(i, _)| i);
    let to = text[from..]
        .char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| from + i);
    (from, to)
}

/// Edits between two words, a swap of neighbours counting as one (as Tantivy's
/// fuzzy matching counts them).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::doc;

    const NOW: i64 = 1_795_148_363_697;

    #[test]
    fn parses_words_phrases_and_filters() {
        let p = parse_query(
            r#"hook error "connection refused" -gopher in:api from:me after:7d"#,
            NOW,
        )
        .unwrap();
        assert_eq!(p.words, vec!["hook", "error"]);
        assert_eq!(p.phrases, vec!["connection refused"]);
        assert_eq!(p.exclude, vec!["gopher"]);
        assert_eq!(p.places, vec!["api"]);
        assert_eq!(p.role, Some("user"));
        assert_eq!(p.after, Some(NOW - 7 * 86_400_000));
        assert!(!p.prefix);
        let typing = parse_query("hook err", NOW).unwrap();
        assert!(typing.prefix);
        assert!(!parse_query("hook err ", NOW).unwrap().prefix);
    }

    #[test]
    fn colons_in_text_are_text() {
        let p = parse_query("http://localhost:7777 std::fs", NOW).unwrap();
        assert_eq!(p.words, vec!["http://localhost:7777", "std::fs"]);
    }

    #[test]
    fn bad_filters_refuse() {
        assert_eq!(
            parse_query("x after:2026-02-30", NOW),
            Err(Refused("after:2026-02-30".into()))
        );
        assert_eq!(
            parse_query("x from:nobody", NOW),
            Err(Refused("from:nobody".into()))
        );
        assert_eq!(
            parse_query("x before:soon", NOW),
            Err(Refused("before:soon".into()))
        );
        assert!(parse_query("x after:2024-02-29", NOW).is_ok());
    }

    fn sample() -> Index {
        let (schema, f) = index::schema();
        let idx = Index::create_in_ram(schema);
        idx.tokenizers()
            .register(index::TOKENIZER, index::analyzer());
        let mut w = idx.writer_with_num_threads(1, 15_000_000).unwrap();
        let rows = [
            (
                "s1",
                "/w/api",
                10u64,
                1000i64,
                "user",
                "Why does the hook fail with connect ECONNREFUSED?",
                "",
            ),
            (
                "s1",
                "/w/api",
                20,
                2000,
                "assistant",
                "The hook posts to a server that is not running.",
                "",
            ),
            (
                "s1",
                "/w/api",
                30,
                3000,
                "attachment",
                "",
                "/tmp/api/hook-ECONNREFUSED-notes.md",
            ),
            (
                "s3",
                "/w/third",
                10,
                7000,
                "attachment",
                "",
                "/home/me/specs/API-TRACE.md",
            ),
            (
                "s2",
                "/w/other",
                10,
                5000,
                "user",
                "Write the release notes for the search feature",
                "",
            ),
            (
                "s2",
                "/w/other",
                40,
                6000,
                "assistant",
                "Release notes are in CHANGELOG.md now.",
                "",
            ),
        ];
        for (s, proj, off, ts, role, body, names) in rows {
            w.add_document(doc!(f.session => s, f.file => format!("/{s}.jsonl"), f.project => proj, f.place => proj,
                f.offset => off, f.ts => ts, f.role => role, f.body => body, f.names => names)).unwrap();
        }
        w.commit().unwrap();
        idx
    }

    fn ask(q: &str) -> Answer {
        search_index(
            &sample(),
            &Ask {
                q,
                now: NOW,
                sessions: 10,
                per_session: 5,
                in_order: None,
                pool: POOL,
                recent: false,
            },
            false,
        )
        .unwrap()
    }

    #[test]
    fn all_words_must_match_and_conversation_outranks_attachments() {
        let a = ask("hook econnrefused ");
        assert_eq!(a.mode, Some(Mode::Exact));
        assert_eq!(a.sessions.len(), 1);
        let hits = &a.sessions[0].hits;
        assert_eq!(hits[0].role, "user");
        assert_eq!(hits.last().unwrap().role, "attachment");
        assert!(
            a.sessions[0].hits.iter().all(|h| h.offset != 20),
            "the reply lacks ECONNREFUSED"
        );
    }

    #[test]
    fn attachments_by_name() {
        let a = ask("API-TRACE.md");
        assert_eq!(a.sessions.len(), 1);
        assert_eq!(a.sessions[0].session, "s3");
        let h = &a.sessions[0].hits[0];
        assert_eq!(h.role, "attachment");
        assert_eq!(h.snippet, "/home/me/specs/API-TRACE.md");
        assert_eq!(ask("trace from:file").sessions.len(), 1);
        assert_eq!(ask("trace from:me").sessions.len(), 0);
    }

    #[test]
    fn newest_first_when_asked() {
        let b = search_index(
            &sample(),
            &Ask {
                q: "the",
                now: NOW,
                sessions: 10,
                per_session: 5,
                in_order: None,
                pool: POOL,
                recent: true,
            },
            false,
        )
        .unwrap();
        let sessions: Vec<&str> = b.sessions.iter().map(|s| s.session.as_str()).collect();
        assert_eq!(sessions, vec!["s2", "s1"], "s2's messages are newer");
        let ts: Vec<i64> = b.sessions[1].hits.iter().map(|h| h.ts).collect();
        assert!(ts.windows(2).all(|w| w[0] >= w[1]), "{ts:?}");
    }

    #[test]
    fn filters_alone_list_what_they_let_through() {
        let a = search_index(
            &sample(),
            &Ask {
                q: "from:me session:s1",
                now: NOW,
                sessions: 1,
                per_session: 1,
                in_order: Some(100),
                pool: POOL,
                recent: false,
            },
            false,
        )
        .unwrap();
        let hits = &a.sessions[0].hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.starts_with("Why does the hook"));
        assert!(ask("   ").sessions.is_empty());
    }

    #[test]
    fn last_word_is_a_prefix_while_typing() {
        let a = ask("release no");
        assert_eq!(a.sessions.len(), 1);
        assert_eq!(a.sessions[0].session, "s2");
    }

    #[test]
    fn typos_fall_back_to_fuzzy() {
        let a = ask("relase notse ");
        assert_eq!(a.mode, Some(Mode::Fuzzy));
        assert_eq!(a.sessions[0].session, "s2");
    }

    #[test]
    fn filters_narrow() {
        assert_eq!(ask("hook in:api").sessions.len(), 1);
        assert_eq!(ask("hook in:other").sessions.len(), 0);
        let mine = ask("hook from:me ");
        assert!(mine.sessions[0].hits.iter().all(|h| h.role == "user"));
        assert_eq!(ask("release notes after:1970-01-01").sessions.len(), 1);
        assert_eq!(ask("release notes before:1970-01-01").sessions.len(), 0);
        assert_eq!(
            ask("hook -server ").sessions[0]
                .hits
                .iter()
                .filter(|h| h.role == "assistant")
                .count(),
            0
        );
        assert_eq!(ask("x from:nobody").refused.as_deref(), Some("from:nobody"));
    }

    #[test]
    fn in_session_hits_come_in_log_order() {
        let a = search_index(
            &sample(),
            &Ask {
                q: "hook session:s1",
                now: NOW,
                sessions: 1,
                per_session: 1,
                in_order: Some(100),
                pool: POOL,
                recent: false,
            },
            false,
        )
        .unwrap();
        let offs: Vec<u64> = a.sessions[0].hits.iter().map(|h| h.offset).collect();
        assert_eq!(offs, vec![10, 20, 30]);
    }

    #[test]
    fn highlights_point_at_the_words_in_js_offsets() {
        let a = ask("econnrefused ");
        let h = &a.sessions[0].hits[0];
        let units: Vec<u16> = h.snippet.encode_utf16().collect();
        let [s, e] = h.highlights[0];
        assert_eq!(String::from_utf16(&units[s..e]).unwrap(), "ECONNREFUSED");
    }

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("relase", "release"), 1);
        assert_eq!(edit_distance("notse", "notes"), 1);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
