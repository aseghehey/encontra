---
name: encontra
description: Search and read the user's past Claude Code conversations (~/.claude/projects session logs) with the encontra CLI. Use when asked what was said, decided or done in an earlier session, to find a past conversation, to analyse conversation history, or to read part of a long session log. Prefer it over grep or reading .jsonl files directly; logs can be hundreds of MB.
---

# encontra: search Claude Code conversation logs

`encontra` indexes `~/.claude/projects/**/*.jsonl` and answers in JSON. Read only:
it never changes a conversation (except `rename`, and only when asked).

Check it's installed: `command -v encontra || ls ~/.local/bin/encontra`. If not,
tell the user how to install it (https://github.com/aseghehey/encontra#install)
instead of grepping raw logs.

## Workflow

1. **Find candidate sessions**
   - By topic: `encontra query --json --update "<words>"`
   - By recency or title: `encontra sessions --update --limit 50 [--filter <text>]`
2. **Search inside one session**: `encontra query --json --in-order 200 "<words> session:<id>"`
3. **Read around a hit**: each hit has `file` and `offset` (a byte offset).
   `encontra page --file <file> --from <offset - 200000> --to <offset + 200000>`
   For the end of a session: `encontra page --file <file> --tail 2000000`.
   Keep pages to about 2 MB; page again for more.
4. Analyse what you read. Quote the user's and the assistant's words when
   it matters, with the session and its date.

Use `--update` on the first call so the latest conversations are indexed (it
spends at most 20 s). Later calls in the same task can skip it.

## Query syntax

- `words` all must match (the last may be a prefix); typos fall back to fuzzy
  matching (`"mode": "fuzzy"` in the answer).
- `"exact phrase"`, `-excluded`
- `from:me` (the user), `from:claude` (the assistant), `from:files` (attached file names)
- `after:2026-11-01`, `before:7d` (also `12h`, `2w`)
- `in:<part of the project path>`, `session:<session id>`
- `--recent` ranks newer first; `--sessions N` and `--hits N` raise the limits
  (defaults 20 and 3).

An unreadable filter refuses the search (`"refused": "<token>"`): fix the token.

## Output

- `query`: `{mode, total, refused, sessions: [{session, project, file, last_ts, matches, hits: [{offset, ts, role, snippet}]}]}`
- `sessions`: `{sessions: [{session, project, title, name, first, first_ts, last_ts, messages, file, size, live}]}`; times are Unix ms; `live` is set for a running session.
- `page`: `{from, to, size, lines: [{at, line}]}`; each `line` is one raw JSONL
  record (`type` user / assistant / system, `message.content`, `timestamp`).
  Long tool output is cut and bookkeeping records are left out.
- Errors: `{"error": "..."}` and a non-zero exit.

Sub-agent turns aren't in `page`. For those, read
`<session file without .jsonl>/subagents/agent-*.jsonl` with `tail -c`.

## Other machines

On a remote machine with its own logs, run it there:
`ssh <host> '~/.local/bin/encontra query --json "<words>"'`.
