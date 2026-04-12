use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::types::{ByteRange, HeadlineEntry, Position, QueryMatch};

/// Walk a node's ancestors and collect headline titles, innermost last.
fn breadcrumbs<'a>(node: Node<'a>, source: &[u8]) -> Vec<String> {
    let mut crumbs = Vec::new();
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "section" {
            if let Some(headline) = n.child_by_field_name("headline") {
                if let Some(item) = headline.child_by_field_name("item") {
                    if let Ok(text) = item.utf8_text(source) {
                        crumbs.push(text.trim().to_string());
                    }
                }
            }
        }
        current = n.parent();
    }
    crumbs.reverse();
    crumbs
}

fn position(p: tree_sitter::Point) -> Position {
    Position { row: p.row, column: p.column }
}

fn range(n: &Node) -> ByteRange {
    ByteRange { start: n.start_byte(), end: n.end_byte() }
}

/// Run an arbitrary tree-sitter S-expression query against parsed source.
/// Returns one [`QueryMatch`] per capture per match.
pub fn run_query(
    source: &[u8],
    tree: &tree_sitter::Tree,
    query_src: &str,
) -> Result<Vec<QueryMatch>> {
    let language = tree_sitter_org::language();
    let query = Query::new(&language, query_src)
        .context("failed to compile query")?;
    let mut cursor = QueryCursor::new();

    let mut results = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(qmatch) = matches.next() {
        for capture in qmatch.captures {
            let node = capture.node;
            let name = query.capture_names()[capture.index as usize].to_string();
            let text = node.utf8_text(source).unwrap_or("").to_string();
            results.push(QueryMatch {
                capture: name,
                text,
                range: range(&node),
                start_position: position(node.start_position()),
                end_position: position(node.end_position()),
                breadcrumbs: breadcrumbs(node, source),
            });
        }
    }
    Ok(results)
}

/// Extract the document outline as a flat list of headlines.
pub fn outline(source: &[u8], tree: &tree_sitter::Tree) -> Result<Vec<HeadlineEntry>> {
    let query_src = r#"
        (section
          headline: (headline
            stars: (stars) @stars
            item: (item) @title
            tags: (tag_list (tag) @tag)?))
    "#;
    let language = tree_sitter_org::language();
    let query = Query::new(&language, query_src).context("outline query")?;
    let mut cursor = QueryCursor::new();

    let stars_idx = query.capture_index_for_name("stars").unwrap();
    let title_idx = query.capture_index_for_name("title").unwrap();
    let tag_idx   = query.capture_index_for_name("tag").unwrap();

    let mut entries: Vec<HeadlineEntry> = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(qmatch) = matches.next() {
        let mut stars_text = "";
        let mut title_node: Option<Node> = None;
        let mut tags: Vec<String> = Vec::new();

        for cap in qmatch.captures {
            if cap.index == stars_idx {
                stars_text = cap.node.utf8_text(source).unwrap_or("");
            } else if cap.index == title_idx {
                title_node = Some(cap.node);
            } else if cap.index == tag_idx {
                tags.push(cap.node.utf8_text(source).unwrap_or("").to_string());
            }
        }

        if let Some(title) = title_node {
            let full_title = title.utf8_text(source).unwrap_or("").trim().to_string();
            let depth = stars_text.len();

            // Strip leading TODO keyword if present
            let (keyword, title_text) = split_keyword(&full_title);

            entries.push(HeadlineEntry {
                depth,
                title: title_text,
                todo_keyword: keyword,
                tags,
                range: range(&title),
                start_position: position(title.start_position()),
            });
        }
    }
    Ok(entries)
}

/// Split "TODO My headline" into (Some("TODO"), "My headline").
fn split_keyword(title: &str) -> (Option<String>, String) {
    const KEYWORDS: &[&str] = &["TODO", "DONE", "NEXT", "WAITING", "CANCELLED"];
    let mut parts = title.splitn(2, ' ');
    if let Some(first) = parts.next() {
        if KEYWORDS.contains(&first) {
            let rest = parts.next().unwrap_or("").to_string();
            return (Some(first.to_string()), rest);
        }
    }
    (None, title.to_string())
}
