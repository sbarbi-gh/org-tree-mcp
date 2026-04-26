use anyhow::Result;
use regex::Regex;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use rmcp::handler::server::wrapper::Parameters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use org_parser::{
    ensure_custom_id as org_ensure_custom_id, insert_subtree as org_insert_subtree,
    make_parser, outline, parse_org_link,
    patch_subtree as org_patch_subtree, refile_subtree as org_refile_subtree,
    resolve_section_ref, run_query, validate as org_validate,
    Dest, EnsureCustomIdResult, InsertOutput, OrgLink, QueryMatch, RefileOutput, SectionInfo,
    SectionRef,
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
    /// Each element is a case-insensitive regex.
    heading_path: Option<Vec<String>>,
    /// Value of the :CUSTOM_ID: property of the target section.
    custom_id: Option<String>,
    /// 0-indexed row (start_position.row from outline / query results). Any row
    /// inside the section — including its headline row — resolves to the
    /// innermost enclosing section.
    line: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct PatchSubtreeParams {
    /// Absolute path to the org file.
    file: String,
    /// Value of the :CUSTOM_ID: property of the target section (preferred — use
    /// this whenever the section has one).
    custom_id: Option<String>,
    /// 0-indexed row (start_position.row from outline / query results).
    /// Used when custom_id is absent.
    line: Option<usize>,
    /// Heading path from root to target section; each element is a
    /// case-insensitive regex. Used when both custom_id and line are absent.
    heading_path: Option<Vec<String>>,
    /// Literal string to search for within the section (all occurrences are replaced).
    search: String,
    /// Replacement string.
    replace: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct InsertSubtreeParams {
    /// Org-mode text to insert verbatim. The caller is responsible for
    /// adjusting heading depth to match the target nesting level.
    content: String,
    /// Destination placement. `dest.section.file` (or `dest.file` for
    /// doc_top / doc_bottom) must be an absolute path — there is no source
    /// file to fall back to.
    #[serde(deserialize_with = "from_str_or_obj")]
    dest: Dest,
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

/// Deserialize T from either a proper JSON object or a JSON-encoded string.
/// Works around MCP clients that stringify complex tool parameters.
fn from_str_or_obj<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(deserializer).map_err(serde::de::Error::custom)?;
    match v {
        serde_json::Value::String(s) => serde_json::from_str(&s).map_err(serde::de::Error::custom),
        other => serde_json::from_value(other).map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct RefileSubtreeParams {
    /// Section to move.  `src.file` is always required (no SameFile for src).
    #[serde(deserialize_with = "from_str_or_obj")]
    src: SectionRef,
    /// Destination placement.
    #[serde(deserialize_with = "from_str_or_obj")]
    dest: Dest,
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

    /// Return structured metadata and full org text for a section identified by
    /// exactly one of: custom_id (preferred), line (0-indexed row from outline /
    /// query start_position.row), or heading_path (case-insensitive regex per
    /// level from the document root). Priority: custom_id > line > heading_path.
    #[tool(description = "Get section metadata and full org text by heading_path, custom_id, or line (0-indexed row). Priority when multiple are given: custom_id > line > heading_path.")]
    async fn subtree(&self, Parameters(p): Parameters<SubtreeParams>) -> String {
        let file = p.file.clone();
        let r = if let Some(id) = p.custom_id {
            SectionRef::Id { file: None, id }
        } else if let Some(n) = p.line {
            SectionRef::Line { file: None, line: n }
        } else if let Some(path) = p.heading_path {
            SectionRef::Path { file: None, path }
        } else {
            return error_json("provide at least one of custom_id, line, or heading_path");
        };
        match parse_and_run(&file, |src, tree| {
            let info = resolve_section_ref(src, tree, &r)?;
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
    /// `custom_id` (preferred), `line`, or `heading_path`. All occurrences of
    /// `search` are replaced with `replace`. The file is updated in-place.
    /// Returns the modified subtree text.
    #[tool(description = "Search and replace text within a subtree identified by custom_id (preferred), line, or heading_path, writing the result back to the file.")]
    async fn patch_subtree(&self, Parameters(p): Parameters<PatchSubtreeParams>) -> String {
        let r = if let Some(id) = p.custom_id {
            SectionRef::Id { file: None, id }
        } else if let Some(n) = p.line {
            SectionRef::Line { file: None, line: n }
        } else if let Some(path) = p.heading_path {
            SectionRef::Path { file: None, path }
        } else {
            return error_json("provide at least one of custom_id (preferred), line, or heading_path");
        };
        match run_patch(&p.file, &r, &p.search, &p.replace) {
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

    /// Move a section to a new location in the same or a different org file.
    /// The source section is identified by `src` (CUSTOM_ID preferred; also
    /// accepts line or heading_path).  The source section is deleted from
    /// `src.file`; the depth-adjusted subtree is inserted at the position
    /// described by `dest`.  Both files are written only after validation
    /// succeeds.
    ///
    /// `dest.placement` controls all other `dest` fields:
    ///   - `before` / `after`       — sibling of a section anchor
    ///   - `first_child` / `last_child` — child of a section anchor
    ///   - `doc_top` / `doc_bottom` — top-level position in the file
    ///
    /// If the section has a CUSTOM_ID that already exists in the destination
    /// it is automatically disambiguated (suffix `-2`, `-3`, …).
    ///
    /// Returns `{ src: {file, title}, dest: {file, custom_id, line},
    ///   custom_id_changed, validation: {errors, warnings} }`.
    #[tool(description = "Move a section (by CUSTOM_ID, line, or heading_path — CUSTOM_ID preferred) within or between org files, adjusting heading depth and disambiguating CUSTOM_ID collisions automatically.")]
    async fn refile_subtree(&self, Parameters(p): Parameters<RefileSubtreeParams>) -> String {
        match run_refile(&p.src, &p.dest) {
            Ok(s) => s,
            Err(e) => error_json(&e.to_string()),
        }
    }

    /// Insert raw org-mode text at the destination described by `dest`.
    /// The placement semantics are identical to `refile_subtree`.
    /// `dest.section.file` (or `dest.file` for doc_top / doc_bottom) must
    /// always be an absolute path.  The caller is responsible for
    /// depth-adjusting `content` before calling this tool.
    /// Returns `{ dest: {file, line}, validation: {errors, warnings} }`.
    #[tool(description = "Insert org-mode text at a destination (same placement semantics as refile_subtree). dest.section.file must be set. Caller adjusts heading depth. Returns {dest: {file, line}, validation}.")]
    async fn insert_subtree(&self, Parameters(p): Parameters<InsertSubtreeParams>) -> String {
        match run_insert(&p.content, &p.dest) {
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
literal search-and-replace within a section identified by custom_id (preferred), \
line, or heading_path, writing the result back to the file. Use `ensure_custom_id` \
to add a :CUSTOM_ID: to a section identified by 0-indexed line, with automatic \
disambiguation if the proposed ID is already taken. Use `refile_subtree` to move \
a section (identified by custom_id, line, or heading_path — custom_id strongly \
preferred) within or across org files: the section is removed from src, \
depth-adjusted to fit the destination nesting level, and inserted according to \
`dest.placement` (before/after a sibling, first_child/last_child inside a section, \
or doc_top/doc_bottom at the file root). CUSTOM_ID collisions in the destination \
are auto-disambiguated; the destination line number is returned. Use \
`insert_subtree` to insert raw org text at a destination (same placement \
semantics as refile_subtree, but no source section is removed); the caller \
supplies depth-adjusted content and dest.section.file or dest.file."
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
    match parse_org_link(link)? {
        OrgLink::Section(r) => {
            let file = match r.file() {
                Some(f) => resolve(f)?,
                None => require_base()?,
            };
            let file_for_result = file.clone();
            parse_and_run(&file, move |src, tree| {
                let info = resolve_section_ref(src, tree, &r)?;
                Ok(serde_json::to_string_pretty(&LinkedSection { file: file_for_result, info })?)
            })
        }
        OrgLink::Document(path) => {
            let file = resolve(&path)?;
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
            Ok(serde_json::to_string_pretty(&LinkedFile { file, content })?)
        }
    }
}

fn run_patch(file: &str, r: &SectionRef, search: &str, replace: &str) -> anyhow::Result<String> {
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    let (modified_bytes, new_section) = org_patch_subtree(&source, &tree, r, search, replace)?;
    let report = org_validate(&modified_bytes)?;
    if report.has_errors() {
        anyhow::bail!(
            "write aborted — validation errors: {}",
            report.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
        );
    }
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
    let r = SectionRef::Line { file: None, line };
    let EnsureCustomIdResult { custom_id, subtree, file_content, already_existed } =
        org_ensure_custom_id(&source, &tree, &r, proposed_id)?;
    if !already_existed {
        let report = org_validate(&file_content)?;
        if report.has_errors() {
            anyhow::bail!(
                "write aborted — validation errors: {}",
                report.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
            );
        }
        std::fs::write(file, &file_content)
            .map_err(|e| anyhow::anyhow!("cannot write {file}: {e}"))?;
    }
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "custom_id": custom_id,
        "already_existed": already_existed,
        "subtree": subtree,
    }))?)
}

fn run_refile(src_ref: &SectionRef, dest: &Dest) -> anyhow::Result<String> {
    let RefileOutput {
        src_file,
        dest_file,
        src_bytes,
        dest_bytes,
        final_custom_id,
        custom_id_changed,
        dest_start_line,
        src_title,
        validation,
    } = org_refile_subtree(src_ref, dest)?;

    if validation.has_errors() {
        anyhow::bail!(
            "write aborted — validation errors: {}",
            validation.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
        );
    }

    let same_file = src_file == dest_file;
    if same_file {
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {dest_file}: {e}"))?;
    } else {
        std::fs::write(&src_file, &src_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {src_file}: {e}"))?;
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {dest_file}: {e}"))?;
    }

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "src": { "file": src_file, "title": src_title },
        "dest": {
            "file": dest_file,
            "custom_id": final_custom_id,
            "line": dest_start_line,
        },
        "custom_id_changed": custom_id_changed,
        "validation": validation,
    }))?)
}

fn run_insert(content: &str, dest: &Dest) -> anyhow::Result<String> {
    let InsertOutput { dest_file, dest_bytes, dest_start_line, validation } =
        org_insert_subtree(content, dest)?;
    if validation.has_errors() {
        anyhow::bail!(
            "write aborted — validation errors: {}",
            validation.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
        );
    }
    std::fs::write(&dest_file, &dest_bytes)
        .map_err(|e| anyhow::anyhow!("cannot write {dest_file}: {e}"))?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "dest": { "file": dest_file, "line": dest_start_line },
        "validation": validation,
    }))?)
}

fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

// ── query examples catalogue ──────────────────────────────────────────────────

const QUERY_EXAMPLES: &str = org_parser::QUERY_EXAMPLES;

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
