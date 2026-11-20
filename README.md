# herdr-search

Full-text search over Claude Code session logs, for herdr-chat. A static binary
that runs on each machine where agents run, built on
[Tantivy](https://github.com/quickwit-oss/tantivy). No server: every command
opens the index, answers in one JSON object on stdout and exits.

```sh
herdr-search index                 # bring the index up to date (only new bytes are read)
herdr-search query "hook error"    # sessions with matches, best first, with snippets
herdr-search query --in-order 500 "hook session:<id>"   # every hit in one session, in log order
herdr-search stats
```

Defaults: logs from `~/.claude/projects`, index in
`${XDG_DATA_HOME:-~/.local/share}/herdr-search/index`. `--root` and `--index`
change them.

## What's indexed

Conversation only: the person's prompts (typed, queued mid-turn, slash
commands, `!` shell commands) and the agent's replies, following the chat's own
idea of a message (`mods/chat/parse.js`). Files the person attached (`@path`)
are indexed by **name only**, at a lower weight, so conversation ranks first.
Tool calls and results, thinking, command output, injected context, hook output,
sub-agent turns and the log's bookkeeping are left out. Sub-agent logs
(`<session>/subagents/`) are skipped unless `index --subagents`.

Each document keeps its session, project folder, time and the **byte offset**
of its log line, so a hit can be opened at that exact point of a long log.

Logs only grow, so each file's progress is a byte offset: an update reads what
was appended since, and a file that got shorter is read again from the start.
Progress is saved next to the index (`herdr-search-state.json`), so nothing is
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
cargo build --release                                              # this machine
cargo zigbuild --release --target x86_64-unknown-linux-musl        # a static Linux binary
```
