use anyhow::Result;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use org_parser::{get_subtree, make_parser, outline, run_query, QueryMatch};

// ── parameter types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct OutlineParams {
    /// Absolute path to the org file.
    file: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct QueryParams {
    /// Absolute path to the org file.
    file: String,
    /// Tree-sitter S-expression query string.
    query: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SubtreeParams {
    /// Absolute path to the org file.
    file: String,
    /// Heading title path from root to target section, e.g. ["Results", "PCA"].
    /// Each element is a case-insensitive regex matched against the headline
    /// title (TODO keyword stripped). Use this OR custom_id.
    heading_path: Option<Vec<String>>,
    /// Value of the :CUSTOM_ID: property of the target section.
    /// Use this OR heading_path.
    custom_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SearchDirParams {
    /// Directory to search recursively for *.org files.
    dir: String,
    /// Tree-sitter S-expression query string.
    query: String,
}

// ── per-file match with path ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct FileMatch {
    file: String,
    #[serde(flatten)]
    m: QueryMatch,
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

    /// Run an arbitrary tree-sitter S-expression query against an org file.
    /// Returns a JSON array of matches, each with capture name, text, byte
    /// range, source position, and breadcrumb path through parent headlines.
    ///
    /// Use the `query_examples` tool to see documented patterns for the org
    /// grammar.
    #[tool(description = "Run a tree-sitter S-expression query against an org file.")]
    async fn query(&self, Parameters(p): Parameters<QueryParams>) -> String {
        match parse_and_run(&p.file, |src, tree| {
            let matches = run_query(src, tree, &p.query)?;
            Ok(serde_json::to_string_pretty(&matches)?)
        }) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Return the raw org text of a subtree identified by a heading path or
    /// CUSTOM_ID property. Each heading_path element is a case-insensitive
    /// regex matched against the cleaned headline title (TODO keyword
    /// stripped). Provide exactly one of heading_path or custom_id.
    #[tool(description = "Get the full org text of a subtree by heading path or CUSTOM_ID.")]
    async fn subtree(&self, Parameters(p): Parameters<SubtreeParams>) -> String {
        let heading_path = p.heading_path;
        let custom_id = p.custom_id;
        match parse_and_run(&p.file, |src, tree| {
            get_subtree(src, tree, heading_path.as_deref(), custom_id.as_deref())
        }) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Run a tree-sitter S-expression query across every *.org file in a
    /// directory (recursively). Returns a JSON array of matches, each
    /// annotated with the source file path, capture name, text, byte range,
    /// and breadcrumb path. Use this for bottom-up retrieval across a library
    /// of org files.
    #[tool(description = "Search all *.org files in a directory with a tree-sitter query.")]
    async fn search_dir(&self, Parameters(p): Parameters<SearchDirParams>) -> String {
        match search_directory(&p.dir, &p.query) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Return a catalogue of documented tree-sitter query examples for the
    /// org grammar. Use these as starting points for the `query` and
    /// `search_dir` tools.
    #[tool(description = "List documented tree-sitter query examples for the org grammar.")]
    async fn query_examples(&self) -> String {
        QUERY_EXAMPLES.to_string()
    }
}

#[tool_handler(
    name = "org-tree-mcp",
    version = "0.1.0",
    instructions = "Structural navigation and querying of Org mode files via tree-sitter. \
Use `outline` to orient in a document, `query` to run precise S-expression \
queries, `subtree` to retrieve a section's full text, `search_dir` to find \
matching nodes across a library of org files, and `query_examples` to discover \
useful query patterns. All results include breadcrumb paths showing where in \
the heading hierarchy a match lives. Byte ranges are ephemeral — re-run the \
query if the file may have changed. For stable cross-call references, use \
CUSTOM_ID properties in org section drawers."
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

fn search_directory(dir: &str, query_src: &str) -> anyhow::Result<String> {
    let mut all: Vec<FileMatch> = Vec::new();
    for path in collect_org_files_in(Path::new(dir))? {
        let path_str = path.to_string_lossy().to_string();
        match parse_and_run(&path_str, |src, tree| run_query(src, tree, query_src)) {
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

fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

// ── query examples catalogue ──────────────────────────────────────────────────

const QUERY_EXAMPLES: &str = r#"
# Tree-sitter query examples for the org grammar

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
  name: (expr) @_kw (#eq? @_kw "src")
  . parameter: (expr) @lang
  contents: (contents) @contents)
```

## Source blocks filtered by language
```scheme
(block
  name: (expr) @_kw (#eq? @_kw "src")
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
