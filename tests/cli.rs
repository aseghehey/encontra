//! End-to-end tests of the `encontra` binary: the commands, flags, JSON and
//! exit codes that the programs and agents calling it rely on. Each test
//! builds its own small `~/.claude` (projects, sessions) and index in a
//! temporary folder and runs the real binary against it.

use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SHOP: &str = "aaaa-1111";
const BLOG: &str = "bbbb-2222";

/// A Claude Code folder with two sessions in two projects, and an index
/// beside it. Removed when dropped.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("encontra-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let shop = dir.join("claude/projects/-home-me-code-shop");
        let blog = dir.join("claude/projects/-home-me-code-blog");
        fs::create_dir_all(shop.join(format!("{SHOP}/subagents"))).unwrap();
        fs::create_dir_all(&blog).unwrap();
        fs::create_dir_all(dir.join("claude/sessions")).unwrap();
        let f = Fixture { dir };
        f.write(
            &f.log(SHOP),
            &[
                user(SHOP, "/home/me/code/shop", "2026-11-20T10:00:00Z", "Fix the checkout timeout bug"),
                json!({"type":"file-history-snapshot","messageId":"m1","snapshot":{}}),
                assistant(SHOP, "2026-11-20T10:01:00Z", &[
                    json!({"type":"thinking","thinking":"hiddenthoughtword"}),
                    json!({"type":"text","text":"The checkout timeout came from the retry loop."}),
                    json!({"type":"tool_use","id":"t1","name":"Bash","input":{"command":"grep -r timeout"}}),
                ]),
                json!({"type":"user","sessionId":SHOP,"cwd":"/home/me/code/shop","timestamp":"2026-11-20T10:01:30Z",
                       "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"secrettooloutput"}]}}),
                json!({"type":"ai-title","sessionId":SHOP,"aiTitle":"Checkout timeout"}),
            ],
        );
        f.write(
            &f.log(BLOG),
            &[
                user(
                    BLOG,
                    "/home/me/code/blog",
                    "2026-11-22T09:00:00Z",
                    "Write a post about tantivy",
                ),
                assistant(
                    BLOG,
                    "2026-11-22T09:02:00Z",
                    &[json!({"type":"text","text":"Drafted the tantivy post."})],
                ),
            ],
        );
        f.write(
            &shop.join(format!("{SHOP}/subagents/agent-x.jsonl")),
            &[json!({"type":"user","isSidechain":true,"sessionId":SHOP,"timestamp":"2026-11-20T10:02:00Z",
                     "message":{"role":"user","content":"subagentonlyword"}})],
        );
        f
    }

    fn root(&self) -> PathBuf {
        self.dir.join("claude/projects")
    }

    fn log(&self, session: &str) -> PathBuf {
        let project = if session == SHOP {
            "-home-me-code-shop"
        } else {
            "-home-me-code-blog"
        };
        self.root().join(project).join(format!("{session}.jsonl"))
    }

    fn write(&self, path: &Path, lines: &[Value]) {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(path, text).unwrap();
    }

    fn append(&self, path: &Path, line: Value) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// Runs encontra against this fixture: its JSON answer and exit code.
    fn run(&self, args: &[&str]) -> (Value, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_encontra"))
            .args(args)
            .args([
                "--root",
                self.root().to_str().unwrap(),
                "--index",
                self.dir.join("index").to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let v = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("not JSON ({e}): {text:?}"));
        (v, out.status.code().unwrap_or(-1))
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (v, code) = self.run(args);
        assert_eq!(code, 0, "encontra {args:?} failed: {v}");
        assert!(v.get("error").is_none(), "encontra {args:?}: {v}");
        v
    }

    fn query(&self, q: &str) -> Value {
        self.ok(&["query", "--update", q])
    }

    fn indexed(self) -> Fixture {
        self.ok(&["index"]);
        self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn user(session: &str, cwd: &str, ts: &str, text: &str) -> Value {
    json!({"type":"user","sessionId":session,"cwd":cwd,"timestamp":ts,"message":{"role":"user","content":text}})
}

fn assistant(session: &str, ts: &str, content: &[Value]) -> Value {
    json!({"type":"assistant","sessionId":session,"timestamp":ts,"message":{"role":"assistant","content":content}})
}

fn sessions_of(answer: &Value) -> Vec<String> {
    answer["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn query_finds_the_conversation_and_answers_in_the_documented_shape() {
    let f = Fixture::new("query").indexed();
    let a = f.query("checkout timeout");
    assert_eq!(a["mode"], "exact");
    assert_eq!(a["refused"], Value::Null);
    assert_eq!(sessions_of(&a), vec![SHOP]);
    let s = &a["sessions"][0];
    assert_eq!(s["file"], f.log(SHOP).to_str().unwrap());
    assert_eq!(s["project"], "/home/me/code/shop");
    assert!(s["matches"].as_u64().unwrap() >= 1);
    let hit = &s["hits"][0];
    for key in [
        "file",
        "offset",
        "ts",
        "role",
        "snippet",
        "highlights",
        "score",
    ] {
        assert!(hit.get(key).is_some(), "hit without {key}: {hit}");
    }
    // The offset is where that message's line starts in the log.
    let log = fs::read_to_string(f.log(SHOP)).unwrap();
    let at = hit["offset"].as_u64().unwrap() as usize;
    assert!(
        at == 0 || log.as_bytes()[at - 1] == b'\n',
        "offset {at} isn't a line start"
    );
    assert!(log[at..]
        .lines()
        .next()
        .unwrap()
        .contains("checkout timeout"));
}

#[test]
fn only_the_conversation_is_searched() {
    let f = Fixture::new("conversation").indexed();
    for word in ["secrettooloutput", "hiddenthoughtword", "subagentonlyword"] {
        let a = f.query(word);
        assert_eq!(a["total"], 0, "{word} should not be found: {a}");
    }
    // The person's words and the agent's replies are.
    assert_eq!(sessions_of(&f.query("retry loop")), vec![SHOP]);
    assert_eq!(sessions_of(&f.query("tantivy")), vec![BLOG]);
}

#[test]
fn filters_narrow_and_bad_ones_refuse() {
    let f = Fixture::new("filters").indexed();
    assert_eq!(sessions_of(&f.query("tantivy in:blog")), vec![BLOG]);
    assert_eq!(f.query("tantivy in:shop")["total"], 0);
    let mine = f.query("tantivy from:me");
    assert_eq!(mine["sessions"][0]["hits"][0]["role"], "user");
    let theirs = f.query("tantivy from:claude");
    assert_eq!(theirs["sessions"][0]["hits"][0]["role"], "assistant");
    assert_eq!(f.query("checkout after:2026-11-21")["total"], 0);
    assert_eq!(
        sessions_of(&f.query("checkout before:2026-11-21")),
        vec![SHOP]
    );
    assert_eq!(
        sessions_of(&f.query(&format!("post session:{BLOG}"))),
        vec![BLOG]
    );
    assert!(f.query("\"timeout bug\"")["total"].as_u64().unwrap() >= 1);
    assert_eq!(f.query("tantivy -post")["total"], 0);
    let refused = f.query("checkout after:2026-02-30");
    assert_eq!(refused["refused"], "after:2026-02-30");
}

#[test]
fn a_typo_falls_back_to_fuzzy_matching() {
    let f = Fixture::new("fuzzy").indexed();
    let a = f.query("chekout");
    assert_eq!(a["mode"], "fuzzy");
    assert_eq!(sessions_of(&a), vec![SHOP]);
}

#[test]
fn updates_read_only_what_was_added_and_a_rewritten_log_again() {
    let f = Fixture::new("update").indexed();
    let docs = f.ok(&["stats"])["docs"].as_u64().unwrap();
    assert!(docs >= 4, "{docs}");
    f.append(
        &f.log(SHOP),
        user(
            SHOP,
            "/home/me/code/shop",
            "2026-11-21T08:00:00Z",
            "Add a coupon field",
        ),
    );
    assert_eq!(sessions_of(&f.query("coupon")), vec![SHOP]);
    assert_eq!(
        f.ok(&["stats"])["docs"].as_u64().unwrap(),
        docs + 1,
        "only the new line is added"
    );
    // Nothing new: nothing added.
    f.query("coupon");
    assert_eq!(f.ok(&["stats"])["docs"].as_u64().unwrap(), docs + 1);
    // A log that got shorter is read again from its start.
    f.write(
        &f.log(SHOP),
        &[user(
            SHOP,
            "/home/me/code/shop",
            "2026-11-23T08:00:00Z",
            "Start over",
        )],
    );
    assert_eq!(f.query("retry loop")["total"], 0);
    assert_eq!(sessions_of(&f.query("start over")), vec![SHOP]);
}

#[test]
fn sub_agents_only_when_asked() {
    let f = Fixture::new("subagents");
    f.ok(&["index", "--subagents"]);
    let a = f.ok(&["query", "subagentonlyword"]);
    assert_eq!(
        sessions_of(&a),
        vec![SHOP],
        "found, under its parent session: {a}"
    );
    assert!(a["sessions"][0]["file"]
        .as_str()
        .unwrap()
        .contains("/subagents/"));
    // The session list still names the parent's own log, not the sub-agent's.
    let list = f.ok(&["sessions"]);
    let shop = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == SHOP)
        .unwrap()
        .clone();
    assert_eq!(shop["file"], f.log(SHOP).to_str().unwrap());
}

#[test]
fn page_returns_whole_lines_with_their_offsets() {
    let f = Fixture::new("page");
    let path = f.log(SHOP);
    let file = path.to_str().unwrap();
    let log = fs::read_to_string(&path).unwrap();
    let p = f.ok(&["page", "--file", file]);
    assert_eq!(p["from"], 0);
    assert_eq!(p["to"].as_u64().unwrap(), log.len() as u64);
    assert_eq!(p["size"].as_u64().unwrap(), log.len() as u64);
    let lines = p["lines"].as_array().unwrap();
    for l in lines {
        let at = l["at"].as_u64().unwrap() as usize;
        assert_eq!(
            log[at..].lines().next().unwrap(),
            l["line"].as_str().unwrap(),
            "line at {at}"
        );
    }
    // Bookkeeping isn't paged; the conversation is.
    assert!(lines.iter().all(|l| !l["line"]
        .as_str()
        .unwrap()
        .contains("file-history-snapshot")));
    assert!(lines.iter().any(|l| l["line"]
        .as_str()
        .unwrap()
        .contains("Fix the checkout timeout bug")));

    // The tail: only lines that start in the last bytes.
    let last = log.lines().last().unwrap().len() as u64 + 1;
    let t = f.ok(&["page", "--file", file, "--tail", &last.to_string()]);
    let tail = t["lines"].as_array().unwrap();
    assert_eq!(tail.len(), 1);
    assert!(tail[0]["line"].as_str().unwrap().contains("ai-title"));

    // A range from the middle starts at the next whole line.
    let second = log.find('\n').unwrap() as u64 + 1;
    let r = f.ok(&[
        "page",
        "--file",
        file,
        "--from",
        "3",
        "--to",
        &log.len().to_string(),
    ]);
    assert!(r["from"].as_u64().unwrap() >= second);
}

#[test]
fn follow_returns_new_lines_as_they_are_written() {
    let f = Fixture::new("follow");
    let path = f.log(BLOG);
    let size = fs::metadata(&path).unwrap().len().to_string();
    let file = path.to_str().unwrap().to_string();
    let bin = env!("CARGO_BIN_EXE_encontra");
    let start = Instant::now();
    let child = Command::new(bin)
        .args(["follow", "--file", &file, "--from", &size, "--wait", "10"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    f.append(
        &path,
        user(
            BLOG,
            "/home/me/code/blog",
            "2026-11-22T09:05:00Z",
            "Add a conclusion",
        ),
    );
    let out = child.wait_with_output().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "follow waited for its timeout"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let lines = v["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1, "{v}");
    assert!(lines[0]["line"]
        .as_str()
        .unwrap()
        .contains("Add a conclusion"));
    assert_eq!(lines[0]["at"].as_u64().unwrap().to_string(), size);
    assert_eq!(v["reset"], false);

    // Nothing new within the wait: an empty page where it was.
    let to = v["to"].as_u64().unwrap().to_string();
    let idle = f.ok(&["follow", "--file", &file, "--from", &to, "--wait", "1"]);
    assert_eq!(idle["lines"].as_array().unwrap().len(), 0);
    // A log rewritten shorter says so.
    f.write(
        &path,
        &[user(
            BLOG,
            "/home/me/code/blog",
            "2026-11-23T09:00:00Z",
            "New",
        )],
    );
    let reset = f.ok(&["follow", "--file", &file, "--from", &to, "--wait", "1"]);
    assert_eq!(reset["reset"], true);
}

#[test]
fn sessions_lists_each_with_what_it_knows_and_which_is_running() {
    let f = Fixture::new("sessions");
    // A running session: its process (this test) writes sessions/<pid>.json.
    fs::write(
        f.dir
            .join(format!("claude/sessions/{}.json", std::process::id())),
        json!({"sessionId": SHOP}).to_string(),
    )
    .unwrap();
    let a = f.ok(&["sessions", "--update"]);
    assert_eq!(a["total"], 2);
    let list = a["sessions"].as_array().unwrap();
    assert_eq!(sessions_of(&a), vec![BLOG, SHOP], "newest first");
    let shop = &list[1];
    assert_eq!(shop["title"], "Checkout timeout");
    assert_eq!(shop["first"], "Fix the checkout timeout bug");
    assert_eq!(shop["project"], "/home/me/code/shop");
    assert_eq!(shop["file"], f.log(SHOP).to_str().unwrap());
    assert!(shop["messages"].as_u64().unwrap() >= 2);
    assert!(shop["first_ts"].as_i64().unwrap() <= shop["last_ts"].as_i64().unwrap());
    assert_eq!(
        shop["live"]["pid"].as_u64().unwrap(),
        std::process::id() as u64
    );
    assert_eq!(list[0]["live"], Value::Null);
    // --filter narrows.
    assert_eq!(
        sessions_of(&f.ok(&["sessions", "--filter", "tantivy"])),
        vec![BLOG]
    );
}

#[test]
fn rename_names_a_stopped_session_and_refuses_a_running_one() {
    let f = Fixture::new("rename").indexed();
    f.ok(&[
        "rename",
        "--file",
        f.log(BLOG).to_str().unwrap(),
        "--session",
        BLOG,
        "--name",
        "Search post",
    ]);
    let a = f.ok(&["sessions", "--update"]);
    let blog = a["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session"] == BLOG)
        .unwrap()
        .clone();
    assert_eq!(blog["name"], "Search post");

    fs::write(
        f.dir
            .join(format!("claude/sessions/{}.json", std::process::id())),
        json!({"sessionId": SHOP}).to_string(),
    )
    .unwrap();
    let (v, code) = f.run(&[
        "rename",
        "--file",
        f.log(SHOP).to_str().unwrap(),
        "--session",
        SHOP,
        "--name",
        "x",
    ]);
    assert_ne!(code, 0);
    assert!(v["error"].as_str().is_some(), "{v}");
}

#[test]
fn errors_are_json_with_a_failing_exit_code() {
    let f = Fixture::new("errors");
    for args in [
        &["nope"][..],
        &["page"][..],
        &["follow", "--file", "x"][..],
        &["query", "--sessions", "many", "x"][..],
    ] {
        let (v, code) = f.run(args);
        assert_eq!(code, 1, "{args:?}");
        assert!(
            v["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{args:?}: {v}"
        );
    }
    // Searching before anything was indexed says so.
    let (v, code) = f.run(&["query", "checkout"]);
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("no index"), "{v}");
    let (v, code) = f.run(&["page", "--file", "/no/such/log.jsonl"]);
    assert_eq!(code, 1);
    assert!(v["error"].as_str().is_some());
}

#[test]
fn output_to_a_program_is_json_even_without_the_flag() {
    let f = Fixture::new("json").indexed();
    // stdout here is a pipe, not a terminal: JSON without --json.
    let v = f.ok(&["query", "tantivy"]);
    assert_eq!(sessions_of(&v), vec![BLOG]);
    let s = f.ok(&["stats"]);
    for key in ["docs", "segments", "files", "bytes_on_disk"] {
        assert!(s.get(key).is_some(), "stats without {key}");
    }
}
