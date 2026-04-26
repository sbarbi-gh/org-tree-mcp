use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── section reference ─────────────────────────────────────────────────────────

/// A *request* for a unique section — not a resolved node.
/// The server calls [`crate::resolve_section_ref`] to turn this into a
/// [`SectionInfo`], returning an error if zero or more than one sections match.
///
/// `file` is an absolute path to the org file.  When omitted (`null`) in a
/// `dest` context it resolves to the same file as the source (`SameFile`
/// semantics).  For `src` it is always required.
///
/// Resolution rules:
/// - `"by": "id"` — match `:CUSTOM_ID:` exactly; never ambiguous.
/// - `"by": "line"` — match the innermost section spanning the 0-indexed row;
///   never ambiguous.
/// - `"by": "path"` — case-insensitive regex per heading level; may fail with
///   "ambiguous" when duplicate titles exist — use `line` to disambiguate.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum SectionRef {
    /// Stable reference via `:CUSTOM_ID:` property value.
    Id   { file: Option<String>, id: String },
    /// Positional reference via 0-indexed line number (from `outline` /
    /// `query` `start_position.row`).
    Line { file: Option<String>, line: usize },
    /// Structural reference via heading-path (one regex per level from root).
    Path { file: Option<String>, path: Vec<String> },
}

impl SectionRef {
    /// Explicit file path carried by this reference.
    /// `None` means "same file as the operation's source" (SameFile).
    pub fn file(&self) -> Option<&str> {
        match self {
            SectionRef::Id   { file, .. }
            | SectionRef::Line { file, .. }
            | SectionRef::Path { file, .. } => file.as_deref(),
        }
    }
}

// ── validation ────────────────────────────────────────────────────────────────

/// Classification of a diagnostic produced by [`crate::validate`].
/// `#[non_exhaustive]` so future kinds (linting rules, etc.) can be added
/// without breaking match arms in downstream code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// A tree-sitter ERROR or MISSING node in the parsed AST.
    ParseError,
    /// The same `:CUSTOM_ID:` value appears in two or more sections.
    DuplicateCustomId,
}

/// A single validation finding attached to a byte position in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// 0-indexed line number where the issue was found, if locatable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

/// Structured result of validating an org file buffer.
/// `errors` are fatal (writes are aborted); `warnings` are informational.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationReport {
    pub errors:   Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
}

impl ValidationReport {
    pub fn ok() -> Self { Self::default() }
    pub fn has_errors(&self) -> bool { !self.errors.is_empty() }
}

// ── refile ────────────────────────────────────────────────────────────────────

/// Where to place a refiled section relative to the destination.
///
/// Section-relative variants (`before`, `after`, `first_child`, `last_child`)
/// carry a `section` field — a [`SectionRef`] identifying the anchor.
/// Document-relative variants (`doc_top`, `doc_bottom`) optionally carry a
/// `file` path; when omitted the destination defaults to the source file.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum Dest {
    /// Sibling immediately before the anchor section.
    Before      { section: SectionRef },
    /// Sibling immediately after the anchor section.
    After       { section: SectionRef },
    /// First child of the anchor section.
    FirstChild  { section: SectionRef },
    /// Last child of the anchor section.
    LastChild   { section: SectionRef },
    /// First top-level section of the destination file.
    DocTop      { file: Option<String> },
    /// Last top-level section of the destination file.
    DocBottom   { file: Option<String> },
}

/// Output of [`crate::refile_subtree`]. The caller is responsible for writing
/// `src_bytes` / `dest_bytes` to the paths given by `src_file` / `dest_file`.
#[derive(Debug)]
pub struct RefileOutput {
    /// Resolved absolute path of the source file.
    pub src_file: String,
    /// Resolved absolute path of the destination file (equals `src_file` for
    /// same-file refiling).
    pub dest_file: String,
    /// Modified source file (section removed, blank lines cleaned up).
    /// For same-file refiling this equals `dest_bytes`.
    pub src_bytes: Vec<u8>,
    /// Modified destination file (section inserted with padding).
    pub dest_bytes: Vec<u8>,
    /// The CUSTOM_ID the section carries in its new location, if it had one.
    pub final_custom_id: Option<String>,
    /// True when `final_custom_id` differs from the original because of a
    /// collision with an existing ID in the destination file.
    pub custom_id_changed: bool,
    /// 0-indexed headline row of the inserted section in the new dest file.
    pub dest_start_line: usize,
    /// Title of the moved section (for link construction by the caller).
    pub src_title: String,
    /// Validation report for the modified files.
    pub validation: ValidationReport,
}

/// Output of [`crate::insert_subtree`]. The caller is responsible for writing
/// `dest_bytes` to the path given by `dest_file`.
#[derive(Debug)]
pub struct InsertOutput {
    /// Resolved absolute path of the destination file.
    pub dest_file: String,
    /// Modified destination file (content inserted with padding).
    pub dest_bytes: Vec<u8>,
    /// 0-indexed headline row of the inserted content in the destination file.
    pub dest_start_line: usize,
    /// Validation report for the modified file.
    pub validation: ValidationReport,
}

/// A byte range within a source file, as returned by tree-sitter.
/// Valid only for the parse in which it was produced; use heading path
/// or CUSTOM_ID for durable references across requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

/// A position in a source file (0-indexed row and column).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub row: usize,
    pub column: usize,
}

/// A matched node returned by a query, with its location and ancestry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMatch {
    /// Index of the pattern match this capture belongs to.
    /// All captures from the same match share the same match_id.
    /// Use this to group @lang and @contents (or any multi-capture pattern)
    /// without relying on row arithmetic.
    pub match_id: usize,
    /// Capture name from the query pattern.
    pub capture: String,
    /// The text content of the matched node.
    pub text: String,
    /// Byte range within the file. Ephemeral — re-parse to re-anchor.
    pub range: ByteRange,
    pub start_position: Position,
    pub end_position: Position,
    /// Heading ancestors from document root to immediate parent section,
    /// innermost last. Empty for top-level body nodes.
    pub breadcrumbs: Vec<String>,
    /// Surrounding source lines for context. Width is chosen by a heuristic:
    /// fewer total matches → more lines; closer to a heading → fewer lines
    /// before (the heading already orients the reader).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// A section resolved by line number, as returned by `section_for`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionInfo {
    pub title: String,
    pub depth: usize,
    pub todo_keyword: Option<String>,
    /// Existing :CUSTOM_ID: value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,
    /// Ancestor headings from document root, innermost last. Does not include
    /// this section's own title.
    pub breadcrumbs: Vec<String>,
    /// 0-indexed row of the headline.
    pub start_line: usize,
    /// Byte range of the section within the file. Ephemeral — re-parse to re-anchor.
    pub range: ByteRange,
    /// Full org text of the section (headline + contents + subsections).
    pub subtree: String,
}

/// A heading in the document outline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadlineEntry {
    pub depth: usize,
    pub title: String,
    pub todo_keyword: Option<String>,
    pub tags: Vec<String>,
    pub range: ByteRange,
    pub start_position: Position,
}
