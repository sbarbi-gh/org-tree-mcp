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
