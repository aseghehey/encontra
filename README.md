# encontra

*Portuguese for "finds".*

Fast full-text search over your Claude Code session logs, built for the
programs and agents that read them as much as for people.

Claude Code keeps every conversation in `~/.claude/projects`, one JSONL file
per session. They add up quickly: hundreds of sessions, single logs of several
hundred MB. `encontra` indexes them once ([Tantivy](https://github.com/quickwit-oss/tantivy)),
keeps the index up to date by reading only what was appended, and answers in
one JSON object on stdout.

```sh
encontra query "hook error"            # sessions with matches, best first, with snippets
encontra sessions --update             # recent sessions, and which are running now
encontra page --file LOG --tail 2000000   # the end of a log, a page at a time
encontra follow --file LOG --from 123456  # new lines as soon as they're written
```

## Why another one

There are good apps for browsing agent history. `encontra` is the engine
underneath one: a small static binary with no server, made to be driven by
other programs and by agents.

- **Stable JSON in, JSON out.** Every command prints one JSON object; errors
  are `{"error": "..."}` with a non-zero exit.
- **Byte offsets everywhere.** Each hit carries the file and the byte offset of
  its log line, so a caller opens a 400 MB log at that exact point without
  reading the rest.
- **Paging and following live logs.** `page` reads any byte range (or the tail)
  of a log within an output budget; `follow` waits for new lines and returns
  them as they're written. Together they're enough to show a live
  conversation.
- **Knows what's running.** `sessions` says which sessions have a live Claude
  Code process, its parent processes and its tmux pane.
- **Works over SSH.** Nothing to run in the background: put the binary on a
  remote machine and call it there (`ssh host encontra query ...`).
- **Exact first, fuzzy when needed.** Words must all match; with no results it
  retries with typo tolerance and says so.

## Install

Prebuilt binaries for macOS (Apple silicon) and Linux (x86_64, static) are on
the [releases page](https://github.com/aseghehey/encontra/releases):

```sh
# macOS (Apple silicon)
curl -fsSL https://github.com/aseghehey/encontra/releases/latest/download/encontra-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin
# Linux x86_64
curl -fsSL https://github.com/aseghehey/encontra/releases/latest/download/encontra-x86_64-unknown-linux-musl.tar.gz | tar xz -C ~/.local/bin
```

Or build it:

```sh
cargo install --git https://github.com/aseghehey/encontra
```

The first `encontra index` (or any command with `--update`) builds the index,
newest logs first; after that, updates read only new bytes.

## Using it from an agent

`skills/encontra/SKILL.md` teaches an agent (Claude Code or any other that
reads skills) when and how to use `encontra`: find sessions, search, then page
around the hits instead of reading raw logs. This repository is also a Claude
Code plugin; install it with:

```text
/plugin install encontra --marketplace aseghehey/encontra
```

Or copy the skill by hand: `cp -r skills/encontra ~/.claude/skills/`.

## Commands

```text
encontra index    [--root DIR] [--index DIR] [--subagents] [--threads N] [--budget SECS]
encontra query    [--sessions N] [--hits N] [--pool N] [--in-order N] [--recent] [--update] [--json] <query>
encontra sessions [--limit N] [--filter TEXT] [--update]
encontra page     --file LOG [--from OFFSET | --tail BYTES] [--to OFFSET]
encontra follow   --file LOG --from OFFSET [--wait SECS]
encontra rename   --file LOG --session ID --name NAME
encontra stats
```

Defaults: logs from `~/.claude/projects`, index in
`${XDG_DATA_HOME:-~/.local/share}/encontra/index`; `--root` and `--index`
change them.

- **query** prints `{query, mode, total, refused, sessions: [{session, project,
  file, best, last_ts, matches, hits: [{file, offset, ts, role, snippet,
  highlights, score}]}]}`. At a terminal it prints readable text instead, and
  brings the index up to date first; `--json` forces JSON. `--recent` ranks
  newer sessions first; `--in-order N` returns up to N hits of one session
  (`session:<id>`) in log order.
- **sessions** prints `{sessions: [{session, project, title, name, first,
  first_ts, last_ts, messages, file, size, live: {pid, ancestors, tmux} | null}],
  total}`. Times are Unix milliseconds.
- **page** prints `{from, to, size, lines: [{at, line}]}`: the log lines that
  start in `[from, to)`, each the raw JSONL record with the byte offset it
  starts at. Bookkeeping records are left out, long tool output is cut, and a
  page stays around 3 MB of JSON at most. Sub-agent turns (`isSidechain`) are
  left out; read `<session>/subagents/agent-*.jsonl` for those.
- **follow** waits up to `--wait` seconds (20 by default, 120 at most) for whole
  new lines after `--from` and prints them like `page`, plus `reset: true` when
  the log got shorter (rewritten: read it again).
- **rename** names a session that isn't running, the way Claude Code's
  `/rename` does (a running session is refused: rename it there).
- **stats** prints `{docs, segments, files, bytes_on_disk}`.

## What's indexed

Conversation only: the person's prompts (typed, queued mid-turn, slash
commands, `!` shell commands) and the agent's replies, following what a chat view of the
log shows. Files the person attached (`@path`)
are indexed by **name only**, at a lower weight, so conversation ranks first.
Tool calls and results, thinking, command output, injected context, hook output,
sub-agent turns and the log's bookkeeping are left out. Sub-agent logs
(`<session>/subagents/`) are skipped unless `index --subagents`.

Each document keeps its session, project folder, time and the **byte offset**
of its log line, so a hit can be opened at that exact point of a long log.

Logs only grow, so each file's progress is a byte offset: an update reads what
was appended since, and a file that got shorter is read again from the start.
Progress is saved next to the index (`encontra-state.json`), so nothing is
read twice, across runs and reboots. A resumed session appends to its own log,
so only its new turns are read. An index from another format version is thrown
away and rebuilt. A first build goes newest first and commits as it goes; `--budget SECS` stops
starting new files after that long, and the next run carries on.

## Queries

- Plain words must all match; the last one may be a prefix (`hook err` finds
  "hook error"). Words next to each other rank higher.
- `"quoted words"` match as a phrase; `-word` excludes.
- Filters: `in:<any part of the project path>`, `from:me|agent|file` (`file`:
  attached files only),
  `after:` / `before:` (`2026-11-01`, `7d`, `12h`, `2w`), `session:<id>`.
- A filter whose value can't be understood (`after:2026-02-30`) refuses the
  search (`"refused": "<token>"`) instead of being ignored.
- Nothing found: the same query again with typo tolerance (`"mode": "fuzzy"`).
- Text with colons that isn't a filter (`localhost:7777`, `std::fs`) is text.

Highlights are `[start, end)` offsets into the snippet in UTF-16 code units,
so JavaScript can slice the snippet string with them directly.

## Build

```sh
cargo test
cargo build --release                                          # this machine
cargo zigbuild --release --target x86_64-unknown-linux-musl    # a static Linux binary
```

## Acknowledgements

`encontra` started as the search engine of a [herdr-chat](https://github.com/aseghehey/herdr-chat)
fork. What counts as a message follows the rules of herdr-chat's chat reader
(MIT, © naruse).

Not affiliated with Anthropic. Claude Code is Anthropic's.

## License

MIT
