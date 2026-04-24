use anyhow::Result;
use regex::Regex;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use org_parser::{
    ensure_custom_id as org_ensure_custom_id, make_parser, outline, parse_org_link,
    patch_subtree as org_patch_subtree, run_query, section_for as org_section_for,
    EnsureCustomIdResult, OrgLink, QueryMatch, SectionInfo,
};

// ── parameter types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct OutlineParams {
    /// Absolute path to the org file.
    file: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct QueryParams {
    /// Absolute path to an org file, or a directory to search recursively.
    path: String,
    /// Tree-sitter S-expression query string.
    query: String,
    /// Optional regex patterns applied to matched text after structural
    /// filtering. A result is kept only if its text matches all patterns (AND).
    /// When multiple structural nodes cover the same regex hit, only the node
    /// with the smallest byte range (most specific) is returned.
    patterns: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SubtreeParams {
    /// Absolute path to the org file.
    file: String,
    /// Heading title path from root to target section, e.g. ["Results", "PCA"].
    /// Each element is a case-insensitive regex. May be combined with `line`
    /// for disambiguation when duplicate titles exist in the hierarchy.
    heading_path: Option<Vec<String>>,
    /// Value of the :CUSTOM_ID: property of the target section.
    custom_id: Option<String>,
    /// 0-indexed row (start_position.row from outline / query results). Any row
    /// inside the section — including its headline row — resolves to the
    /// innermost enclosing section. May be combined with heading_path.
    line: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct PatchSubtreeParams {
    /// Absolute path to the org file.
    file: String,
    /// Value of the :CUSTOM_ID: property identifying the target section.
    custom_id: String,
    /// Literal string to search for within the section (all occurrences are replaced).
    search: String,
    /// Replacement string.
    replace: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct EnsureCustomIdParams {
    /// Absolute path to the org file.
    file: String,
    /// 0-indexed row identifying the target section (start_position.row as
    /// reported by outline / query). Any row that falls inside the section —
    /// including its headline row — resolves to the innermost enclosing section.
    line: usize,
    /// Proposed :CUSTOM_ID: value. A numeric suffix (-2, -3, …) is appended
    /// automatically if the ID already exists elsewhere in the file.
    custom_id: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct OpenLinkParams {
    /// Org-mode link string. Accepted forms (with or without outer [[ ]]):
    ///   [[#my-id]]                      — CUSTOM_ID in the same file
    ///   [[file:path/to/file.org::#id]]  — CUSTOM_ID in another file
    ///   [[file:path/to/file.org::*H/S]] — heading path (/ separates levels)
    ///   [[file:path/to/file.org]]       — whole file content
    link: String,
    /// File or directory used to resolve same-file and relative links.
    /// Required for same-file links ([[#id]]); required for relative file
    /// paths. A file path is accepted (its parent directory is used) as well
    /// as a bare directory path.
    base_file: Option<String>,
}

// ── per-file match with path ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct FileMatch {
    file: String,
    #[serde(flatten)]
    m: QueryMatch,
}

/// Result of following a section-targeted org link: SectionInfo fields plus
/// the resolved absolute file path.
#[derive(Debug, Serialize)]
struct LinkedSection {
    file: String,
    #[serde(flatten)]
    info: SectionInfo,
}

/// Result of following a bare file link (no section target).
#[derive(Debug, Serialize)]
struct LinkedFile {
    file: String,
    content: String,
}

// ── server ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct OrgMcpServer;

#[tool_router]
impl OrgMcpServer {
    /// Return a flat ordered list of all headlines in an org file, with depth,
    /// optional TODO keyword, tags, and byte range.
    #[tool(description = "Get the document outline (all headlines) of an org file.")]
    async fn outline(&self, Parameters(p): Parameters<OutlineParams>) -> String {
        match parse_and_run(&p.file, |src, tree| {
            let entries = outline(src, tree)?;
            Ok(serde_json::to_string_pretty(&entries)?)
        }) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Run a tree-sitter S-expression query against an org file or every
    /// *.org file in a directory (recursively). `path` may be a file or a
    /// directory. Returns a JSON array of matches, each annotated with the
    /// source file path, capture name, text, byte range, source position, and
    /// breadcrumb path through parent headlines. Optionally supply `patterns`
    /// to further filter results to nodes whose text matches all patterns
    /// (AND); when multiple structural nodes cover the same hit the most
    /// specific (smallest byte range) is kept.
    ///
    /// Use the `query_examples` tool to see documented patterns for the org
    /// grammar.
    #[tool(description = "Run a tree-sitter S-expression query against an org file or a directory of org files.")]
    async fn query(&self, Parameters(p): Parameters<QueryParams>) -> String {
        let patterns = match compile_patterns(p.patterns.as_deref()) {
            Ok(v) => v,
            Err(e) => return error_json(&e.to_string()),
        };
        if Path::new(&p.path).is_dir() {
            match search_directory(&p.path, &p.query, &patterns) {
                Ok(s) => s,
                Err(e) => error_json(&e.to_string()),
            }
        } else {
            let file = p.path.clone();
            match parse_and_run(&p.path, |src, tree| {
                let matches = run_query(src, tree, &p.query, &patterns)?;
                let file_matches: Vec<FileMatch> = matches
                    .into_iter()
                    .map(|m| FileMatch { file: file.clone(), m })
                    .collect();
                Ok(serde_json::to_string_pretty(&file_matches)?)
            }) {
                Ok(s) => s,
                Err(e) => error_json(&e.to_string()),
            }
        }
    }

    /// Return structured metadata and full org text for a section. Criteria are
    /// AND-ed: supply any combination of heading_path (case-insensitive regex
    /// per level, combined with line to resolve duplicate titles), custom_id,
    /// or line (0-indexed row from outline / query start_position.row). At
    /// least one must be provided. When multiple sections match, the innermost
    /// (smallest byte span) is returned.
    #[tool(description = "Get section metadata and full org text by heading_path, custom_id, line (0-indexed row), or any combination.")]
    async fn subtree(&self, Parameters(p): Parameters<SubtreeParams>) -> String {
        let heading_path = p.heading_path;
        let custom_id = p.custom_id;
        let line = p.line;
        match parse_and_run(&p.file, |src, tree| {
            let info = org_section_for(src, tree, heading_path.as_deref(), custom_id.as_deref(), line)?
                .ok_or_else(|| anyhow::anyhow!("section not found"))?;
            Ok(serde_json::to_string_pretty(&info)?)
        }) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Return a catalogue of documented tree-sitter query examples for the
    /// org grammar. Use these as starting points for the `query` tool.
    #[tool(description = "List documented tree-sitter query examples for the org grammar.")]
    async fn query_examples(&self) -> String {
        QUERY_EXAMPLES.to_string()
    }

    /// Follow an Org-mode link and return structured section metadata (same
    /// shape as `subtree`) for section-targeted links, or `{file, content}`
    /// for bare file links. The resolved absolute file path is always included.
    ///
    /// Supported link forms (with or without outer [[ ]]):
    ///   [[#my-id]]                       — CUSTOM_ID in base_file
    ///   [[file:path/to/file.org::#id]]   — CUSTOM_ID in another file
    ///   [[file:path/to/file.org::*H/S]]  — heading path (/ separates levels)
    ///   [[file:path/to/file.org]]        — whole file, returns {file, content}
    #[tool(description = "Follow an Org-mode link and return structured section metadata (title, depth, custom_id, breadcrumbs, subtree text, resolved file path), or {file, content} for bare file links.")]
    async fn open_link(&self, Parameters(p): Parameters<OpenLinkParams>) -> String {
        match follow_org_link(&p.link, p.base_file.as_deref()) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Apply a literal search-and-replace within the subtree identified by
    /// `custom_id`. All occurrences of `search` are replaced with `replace`.
    /// The file is updated in-place. Returns the modified subtree text.
    #[tool(description = "Search and replace text within the subtree identified by a CUSTOM_ID, writing the result back to the file.")]
    async fn patch_subtree(&self, Parameters(p): Parameters<PatchSubtreeParams>) -> String {
        match run_patch(&p.file, &p.custom_id, &p.search, &p.replace) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Ensure the section containing `line` (0-indexed row) has a `:CUSTOM_ID:`
    /// property. If one already exists it is returned unchanged. Otherwise
    /// `custom_id` is checked for uniqueness across the file; a `-2`, `-3`, …
    /// suffix is appended if needed. The file is updated in-place when a new ID
    /// is inserted. Returns JSON with `custom_id`, `already_existed`, `subtree`.
    #[tool(description = "Ensure the section at the given 0-indexed line has a :CUSTOM_ID:, inserting one (with automatic disambiguation) if absent.")]
    async fn ensure_custom_id(&self, Parameters(p): Parameters<EnsureCustomIdParams>) -> String {
        match run_ensure_custom_id(&p.file, p.line, &p.custom_id) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }
}

#[tool_handler(
    name = "org-tree-mcp",
    version = "0.1.0",
    instructions = "Structural navigation and querying of Org mode files via tree-sitter. \
Use `outline` to orient in a document, `query` to run precise S-expression \
queries against a single file or an entire directory of org files, `subtree` \
to retrieve a section's full metadata and text, `open_link` to follow an \
Org-mode link (CUSTOM_ID, heading path, or bare file) and retrieve structured \
section metadata (same shape as `subtree`, with resolved file path) or \
{file, content} for bare file links, and \
`query_examples` to discover useful query patterns. `query` accepts an \
optional `patterns` list to filter results by regex after structural matching \
(all patterns must match — AND semantics); when the same text span is covered \
by multiple nodes the most specific one is kept. All query results include the source file path, breadcrumb paths, and a \
context snippet; the snippet width adapts to match density and proximity to \
the nearest heading. Byte ranges are ephemeral — re-run the query if the file \
may have changed. For stable cross-call references, use CUSTOM_ID properties \
in org section drawers. Use `subtree` to retrieve a section's full metadata and org text; it accepts \
any combination of heading_path, custom_id, or line (0-indexed row from \
outline/query start_position.row) as AND-ed criteria — supply line alongside \
heading_path to resolve duplicate titles. Use `patch_subtree` to apply a \
literal search-and-replace within a section identified by CUSTOM_ID, writing \
the result back to the file. Use `ensure_custom_id` to add a :CUSTOM_ID: to \
a section identified by 0-indexed line, with automatic disambiguation if the \
proposed ID is already taken."
)]
impl ServerHandler for OrgMcpServer {}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_and_run<F, T>(file: &str, f: F) -> anyhow::Result<T>
where
    F: FnOnce(&[u8], &tree_sitter::Tree) -> anyhow::Result<T>,
{
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    f(&source, &tree)
}

fn compile_patterns(raw: Option<&[String]>) -> anyhow::Result<Vec<Regex>> {
    raw.unwrap_or(&[])
        .iter()
        .map(|p| Regex::new(p).map_err(|e| anyhow::anyhow!("invalid pattern {p:?}: {e}")))
        .collect()
}

fn search_directory(dir: &str, query_src: &str, patterns: &[Regex]) -> anyhow::Result<String> {
    let mut all: Vec<FileMatch> = Vec::new();
    for path in collect_org_files_in(Path::new(dir))? {
        let path_str = path.to_string_lossy().to_string();
        match parse_and_run(&path_str, |src, tree| run_query(src, tree, query_src, patterns)) {
            Ok(matches) => {
                for m in matches {
                    all.push(FileMatch { file: path_str.clone(), m });
                }
            }
            Err(e) => eprintln!("warn: skipping {path_str}: {e}"),
        }
    }
    Ok(serde_json::to_string_pretty(&all)?)
}

fn collect_org_files_in(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    collect_org_files(dir, &mut files)?;
    Ok(files)
}

fn collect_org_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read dir {}: {e}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_org_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("org") {
            out.push(path);
        }
    }
    Ok(())
}

fn follow_org_link(link: &str, base_file: Option<&str>) -> anyhow::Result<String> {
    let resolve = |f: &str| -> anyhow::Result<String> {
        if Path::new(f).is_absolute() {
            return Ok(f.to_string());
        }
        let base = base_file.ok_or_else(|| {
            anyhow::anyhow!("base_file required to resolve relative path {f:?}")
        })?;
        let base_path = Path::new(base);
        let base_dir = if base_path.is_dir() {
            base_path
        } else {
            base_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cannot determine parent dir of {base:?}"))?
        };
        Ok(base_dir.join(f).to_string_lossy().into_owned())
    };
    let require_base = || {
        base_file
            .ok_or_else(|| anyhow::anyhow!("base_file required for same-file link"))
            .map(str::to_string)
    };
    let section_json = |file: String, heading_path: Option<Vec<String>>, custom_id: Option<String>| -> anyhow::Result<String> {
        let file_for_result = file.clone();
        parse_and_run(&file, move |src, tree| {
            let info = org_section_for(src, tree, heading_path.as_deref(), custom_id.as_deref(), None)?
                .ok_or_else(|| anyhow::anyhow!("section not found"))?;
            Ok(serde_json::to_string_pretty(&LinkedSection { file: file_for_result, info })?)
        })
    };

    match parse_org_link(link)? {
        OrgLink::SameFileId(id) => {
            section_json(require_base()?, None, Some(id))
        }
        OrgLink::File(f) => {
            let file = resolve(&f)?;
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
            Ok(serde_json::to_string_pretty(&LinkedFile { file, content })?)
        }
        OrgLink::FileId { file: f, id } => {
            section_json(resolve(&f)?, None, Some(id))
        }
        OrgLink::FilePath { file: f, path } => {
            section_json(resolve(&f)?, Some(path), None)
        }
    }
}

fn run_patch(file: &str, custom_id: &str, search: &str, replace: &str) -> anyhow::Result<String> {
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    let (modified_bytes, new_section) = org_patch_subtree(&source, &tree, custom_id, search, replace)?;
    std::fs::write(file, &modified_bytes)
        .map_err(|e| anyhow::anyhow!("cannot write {file}: {e}"))?;
    Ok(new_section)
}

fn run_ensure_custom_id(file: &str, line: usize, proposed_id: &str) -> anyhow::Result<String> {
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    let EnsureCustomIdResult { custom_id, subtree, file_content, already_existed } =
        org_ensure_custom_id(&source, &tree, line, proposed_id)?;
    if !already_existed {
        std::fs::write(file, &file_content)
            .map_err(|e| anyhow::anyhow!("cannot write {file}: {e}"))?;
    }
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "custom_id": custom_id,
        "already_existed": already_existed,
        "subtree": subtree,
    }))?)
}

fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

// ── query examples catalogue ──────────────────────────────────────────────────

const QUERY_EXAMPLES: &str = r#"
# Tree-sitter query examples for the org grammar

## Note on `expr` predicates

`expr` is the grammar's atomic text token (a whitespace-delimited word). Text
predicates (`#eq?`, `#match?`) on `expr` only work reliably when the `expr`
node appears inside a structural parent. A bare `(expr) @e (#eq? @e "X")`
at the root of a query is split into two internal patterns by the tree-sitter
query compiler, causing predicates to be silently ignored.

**Workaround**: wrap in a wildcard parent `(_ ...)`, or use a known structural
parent. The wildcard costs nothing in practice:

```scheme
; BROKEN — predicate ignored, returns every expr in the document
(expr) @e (#eq? @e "CUSTOM_ID")

; CORRECT — returns only expr nodes equal to "CUSTOM_ID"
(_ (expr) @e (#eq? @e "CUSTOM_ID"))
```

The examples below follow this rule throughout.

---

## All headlines (outline)
Returns every section with its stars (depth), title, and tags.

```scheme
(section
  headline: (headline
    stars: (stars) @stars
    item: (item) @title
    tags: (tag_list (tag) @tag)?))
```

## Headlines with a specific tag
```scheme
(section
  headline: (headline
    item: (item) @title
    tags: (tag_list
      (tag) @tag
      (#eq? @tag "notebook"))))
```

## TODO / DONE headlines
First expr in item is the keyword (anchored with `.`).
```scheme
(section
  headline: (headline
    stars: (stars) @stars
    item: (item
      . (expr) @keyword
      (#any-of? @keyword "TODO" "DONE" "NEXT" "WAITING" "CANCELLED")) @title))
```

## Headlines matching a text pattern
```scheme
(section
  headline: (headline
    item: (item) @title
    (#match? @title "experiment")))
```

## All source blocks (language + body)
`@lang` is the language token (anchored with `.` to avoid capturing
header args like `:tangle`); `@contents` is the block body.
```scheme
(block
  name: (expr) @_kw (#match? @_kw "(?i)^src$")
  . parameter: (expr) @lang
  contents: (contents) @contents)
```

## Source blocks filtered by language
```scheme
(block
  name: (expr) @_kw (#match? @_kw "(?i)^src$")
  . parameter: (expr) @lang (#eq? @lang "python")
  contents: (contents) @contents)
```

## Property drawer entries
```scheme
(property_drawer
  (property
    name: (expr) @prop_name
    value: (value) @prop_value))
```

## Specific property by name (e.g. CUSTOM_ID)
```scheme
(property_drawer
  (property
    name: (expr) @_n (#eq? @_n "CUSTOM_ID")
    value: (value) @custom_id))
```

## File-level directives (#+KEY: value)
```scheme
(directive
  name: (expr) @name
  value: (value)? @value)
```

## Org-mode file links ([[file:...]])
Bracket links are tokenised as a single `expr` spanning `[[file:...][desc`.
Use the wildcard parent so the `#match?` predicate is not silently ignored.
```scheme
(_ (expr) @link (#match? @link "^\\[\\[file:"))
```

## SCHEDULED / DEADLINE / CLOSED timestamps
```scheme
(plan
  (entry
    name: (entry_name) @keyword
    timestamp: (timestamp
      date: (date) @date
      day: (day)? @day
      time: (time)? @time) @ts))
```

## Active timestamps anywhere in the document
```scheme
(timestamp "<"
  date: (date) @date
  day: (day)? @day
  time: (time)? @time) @ts
```

## Checklist items (all)
```scheme
(listitem
  checkbox: (checkbox) @checkbox
  contents: (paragraph) @text)
```

## Unchecked items only
```scheme
(listitem
  checkbox: (checkbox) @checkbox
  (#not-match? @checkbox "x|X|\\-")
  contents: (paragraph) @text)
```
"#;

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let transport = rmcp::transport::io::stdio();
    OrgMcpServer
        .serve(transport)
        .await?
        .waiting()
        .await?;
    Ok(())
}
