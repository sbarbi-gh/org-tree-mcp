---
name: org-tree
description: Use this skill when working with Org-mode (.org) files in this project — navigating outlines, running structural tree-sitter queries, reading/patching sections, moving (refiling) sections, inserting content, following org-mode links, or finding/auditing backlinks between org files. Invokes the `org` CLI binary directly via Bash instead of going through the MCP server. Trigger on: "org file", ".org", "org-mode", "outline", "refile", "CUSTOM_ID", "subtree", "backlinks", "check links", "org query".
---

# org-tree: structural Org-mode navigation and editing via the `org` CLI

## Overview

This project ships a Rust core library (`org-parser`) that parses `.org`
files with tree-sitter and exposes structural navigation/editing operations.
The same logic is available two ways: an MCP server (`org-mcp`) and a CLI
binary (`org`). Prefer the CLI in this skill — invoke it directly via Bash.
It requires no running server, and its plain-text output is easy to read
directly; pass `-j`/`--json` when you need to parse the output programmatically.

## Building / locating the binary

```bash
cargo build --release -p org-cli
# binary at target/release/org
```
Or, for iterative work, `cargo run -p org-cli --` works without a separate
build step. Check `which org` first in case it's already installed on PATH.

## Output modes

Every subcommand prints human-readable plain text by default. Add `-j` (or
`--json`, works before or after the subcommand) to get raw JSON instead:

```bash
org outline notes.org          # plain text — good for orienting yourself
org outline notes.org -j       # JSON — good for piping into jq / further parsing
```

**Gotcha:** plain text renders 1-indexed line numbers (matching what you'd see
in an editor); JSON keeps the underlying 0-indexed `row`/`line`/`start_line`
values used internally by the parser. Don't mix the two when computing a line
number by hand.

## Command reference

| Command | What it does |
|---|---|
| `org outline <file>` | Flat list of all headlines (depth, TODO keyword, tags, row) — the entry point for orienting in a document. |
| `org query <path> <query> [-p <pattern>]...` | Run a tree-sitter S-expression query against a file or every `*.org` file in a directory. `-p`/`--pattern` adds regex text filters (AND semantics, applied after structural matching). |
| `org query-examples` | Print documented query patterns for the org grammar — a live reference, same content as `README.org`'s Query Examples section. |
| `org subtree <file> [--id ID \| --line N \| --heading H...]` | Full metadata + org text for one section, identified by CUSTOM_ID (preferred), 0-indexed line, or heading path (repeatable `--heading`, one per level, case-insensitive regex). |
| `org open-link <link> [--base FILE]` | Follow an Org-mode link (`[[#id]]`, `[[file:f.org::#id]]`, `[[file:f.org::*Heading/Sub]]`, `[[file:f.org]]`) and print the target section or file content. `--base` resolves same-file and relative links. |
| `org patch-subtree <file> [--id\|--line\|--heading] --search S --replace R [--force]` | Literal search-and-replace within one section, written back to disk after re-parse validation. Prints a unified diff. |
| `org ensure-custom-id <file> --line N --id ID [--force]` | Add a `:CUSTOM_ID:` to the section at `line` if it doesn't have one (auto-disambiguates with `-2`, `-3`, … on collision). |
| `org refile <src_file> [--src-id\|--src-line\|--src-heading] --placement P [--dest-file F] [--dest-id\|--dest-line] [--force]` | Move a section within or across files. `--placement` is one of `before\|after\|first-child\|last-child\|doc-top\|doc-bottom`. |
| `org insert <content> --placement P --dest-file F [--dest-id\|--dest-line] [--force]` | Insert raw org text at a destination (same placement semantics as `refile`, no source section removed). Caller must depth-adjust `content` before calling. |
| `org backlinks <target_file> <root> [--id ID] [--heading H...]` | Find every link under `root` that points at `target_file` (optionally narrowed to one section). Read-only. |
| `org check-links <root>` | Scan every `.org` file under `root` for links that fail to resolve (dangling file paths, missing CUSTOM_IDs, missing headings). Read-only. |

## CUSTOM_ID discipline

CUSTOM_ID is the preferred, stable way to locate a section — it survives
edits elsewhere in the document and is never ambiguous. Line and heading path
are fallbacks (heading path can be ambiguous with duplicate titles; use
`--line` alongside `--heading` to disambiguate, or just use `--line` alone).
Before patching or refiling a section that lacks one, run `ensure-custom-id`
first.

## The Emacs lockfile guardrail

Every mutating command (`patch-subtree`, `ensure-custom-id`, `refile`,
`insert`) refuses to write to a file that currently has an Emacs lock symlink
(`.#filename` next to it) — this means the file is open for editing in a live
Emacs buffer, and writing to it risks a silent overwrite or corrupting
unsaved edits. The error names the lock owner when readable
(`user@host.pid`). Pass `--force` only when you're sure it's safe (e.g. a
stale lock left behind by a crashed Emacs session) — never pass it reflexively
to work around a real conflict.

## Top-down workflow

Use this when you already know roughly where you're going in a document:

1. **Broad query** — `org outline file.org` (or `-j` if you'll parse it) to see the document's structure.
2. **Focus query** — `org subtree file.org --id SECTION_ID` (or `--line`/`--heading`) to read one section in full.
3. **Patch** — `org patch-subtree file.org --id SECTION_ID --search "..." --replace "..."`. The result is validated before writing; the write aborts on syntax errors or duplicate CUSTOM_IDs.
4. **Insert or refile** — `org insert "* New Heading\n..." --placement after --dest-id SECTION_ID --dest-file file.org`, or `org refile src.org --src-id ID --placement doc-bottom --dest-file dest.org` to move a section elsewhere.

## Horizontal / bottom-up workflow

Use this when exploring across many files:

1. **Search and collect** — `org query notes/ '(section headline: (headline item: (item) @title))' -p keyword1 -p keyword2 -j` to collect matches (with breadcrumbs, file paths, line numbers) across a whole directory.
2. **Focus** — pick interesting hits from the results.
3. **Navigate** — `org open-link "[[file:other.org::#some-id]]" --base notes/` to follow references and explore related sections.
4. **Patch** — as above, targeted edits validated before writing.
5. **Insert or refile** — as above, to place new content or relocate sections.

## Backlinks and check-links for notebook refactoring

Before renaming, moving, or restructuring a section or file, audit what
points at it:

```bash
org backlinks notes/old-name.org notes/          # who links to this file?
org backlinks notes/old-name.org notes/ --id some-section  # narrow to one section
org check-links notes/                            # any already-broken links anywhere?
```

These are **read-only** today — they report, they don't rewrite. There is no
automatic backlink-fixing when you move a file or refile a section yet
(planned future work); run `check-links` again after a manual refactor to
confirm nothing broke.

## Gotchas

- Byte ranges and line numbers from `query`/`outline`/`subtree` are ephemeral — re-run after any edit to the file before reusing them.
- Diffs (`patch-subtree`, `ensure-custom-id`) always print as a standard unified diff, in both plain-text and JSON mode (JSON wraps it in `{"file": ..., "diff": "..."}`).
- Prefer `--id` over `--line` for any reference you'll reuse across multiple commands — line numbers shift as the file changes.
- `org query-examples` and `README.org`'s "Query Examples" section have the same content — use whichever is more convenient.
