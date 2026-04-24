use serde::{Deserialize, Serialize};

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
