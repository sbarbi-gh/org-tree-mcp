use anyhow::{Context, Result, bail};
use regex::Regex;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::types::{ByteRange, HeadlineEntry, Position, QueryMatch};

// ── helpers ──────────────────────────────────────────────────────────────────

fn position(p: tree_sitter::Point) -> Position {
    Position { row: p.row, column: p.column }
}

fn byte_range(n: &Node) -> ByteRange {
    ByteRange { start: n.start_byte(), end: n.end_byte() }
}

/// Collect all `section` → `headline` ancestors of `node`, innermost last.
fn breadcrumbs(node: Node, source: &[u8]) -> Vec<String> {
    let mut crumbs = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "section" {
            if let Some(hl) = n.child_by_field_name("headline") {
                if let Some(item) = hl.child_by_field_name("item") {
                    if let Ok(text) = item.utf8_text(source) {
                        crumbs.push(text.trim().to_string());
                    }
                }
            }
        }
        cur = n.parent();
    }
    crumbs.reverse();
    crumbs
}

/// Split "TODO My headline" → (Some("TODO"), "My headline").
fn split_keyword(title: &str) -> (Option<String>, String) {
    const KW: &[&str] = &["TODO", "DONE", "NEXT", "WAITING", "CANCELLED"];
    if let Some((first, rest)) = title.split_once(' ') {
        if KW.contains(&first) {
            return (Some(first.to_string()), rest.to_string());
        }
    }
    (None, title.to_string())
}

fn tags_from_headline<'a>(headline: Node<'a>, source: &[u8]) -> Vec<String> {
    let Some(tag_list) = headline.child_by_field_name("tags") else {
        return Vec::new();
    };
    let mut cursor = tag_list.walk();
    tag_list
        .children(&mut cursor)
        .filter(|n| n.kind() == "tag")
        .filter_map(|n| n.utf8_text(source).ok())
        .map(String::from)
        .collect()
}

// ── outline ───────────────────────────────────────────────────────────────────

/// Recursively collect all headlines via direct AST traversal.
fn collect_headlines(node: Node, source: &[u8], entries: &mut Vec<HeadlineEntry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "section" {
            if let Some(headline) = child.child_by_field_name("headline") {
                let stars = headline
                    .child_by_field_name("stars")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                let item = headline.child_by_field_name("item");
                if let Some(item_node) = item {
                    let full = item_node.utf8_text(source).unwrap_or("").trim().to_string();
                    let (keyword, title) = split_keyword(&full);
                    entries.push(HeadlineEntry {
                        depth: stars.len(),
                        title,
                        todo_keyword: keyword,
                        tags: tags_from_headline(headline, source),
                        range: byte_range(&child),
                        start_position: position(headline.start_position()),
                    });
                }
            }
            collect_headlines(child, source, entries);
        }
    }
}

/// Return a flat ordered list of all headlines in the document.
pub fn outline(source: &[u8], tree: &Tree) -> Result<Vec<HeadlineEntry>> {
    let mut entries = Vec::new();
    collect_headlines(tree.root_node(), source, &mut entries);
    Ok(entries)
}

// ── run_query ─────────────────────────────────────────────────────────────────

/// Execute an arbitrary tree-sitter S-expression query.
/// Returns one [`QueryMatch`] per capture per pattern match.
pub fn run_query(source: &[u8], tree: &Tree, query_src: &str) -> Result<Vec<QueryMatch>> {
    let language = tree_sitter_org::language();
    let query = Query::new(&language, query_src).context("failed to compile query")?;
    let mut cursor = QueryCursor::new();
    // Remove the default in-progress-match cap. When a pattern has multiple
    // predicates on different captures (e.g. #eq? @lang + #match? @contents),
    // tree-sitter tracks many partial matches simultaneously and silently drops
    // results once the limit is hit. u32::MAX disables the cap.
    cursor.set_match_limit(u32::MAX);

    let mut results = Vec::new();
    let mut match_id = 0usize;
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(qmatch) = matches.next() {
        for capture in qmatch.captures {
            let node = capture.node;
            let name = query.capture_names()[capture.index as usize].to_string();
            // Skip internal captures (prefixed with _) from results
            if name.starts_with('_') {
                continue;
            }
            let text = node.utf8_text(source).unwrap_or("").to_string();
            results.push(QueryMatch {
                match_id,
                capture: name,
                text,
                range: byte_range(&node),
                start_position: position(node.start_position()),
                end_position: position(node.end_position()),
                breadcrumbs: breadcrumbs(node, source),
            });
        }
        match_id += 1;
    }
    if cursor.did_exceed_match_limit() {
        eprintln!("warn: query match limit exceeded — results may be incomplete");
    }
    Ok(results)
}

// ── get_subtree ───────────────────────────────────────────────────────────────

/// Find a section node by walking a heading-title path.
/// Each element of `path` is compiled as a case-insensitive regex and matched
/// against the cleaned headline title (TODO keyword stripped).
fn find_section_by_path<'a>(
    root: Node<'a>,
    source: &[u8],
    path: &[String],
) -> Result<Option<Node<'a>>> {
    if path.is_empty() {
        return Ok(None);
    }
    let pat = Regex::new(&format!("(?i){}", path[0]))
        .with_context(|| format!("invalid heading regex: {:?}", path[0]))?;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "section" {
            continue;
        }
        let title = child
            .child_by_field_name("headline")
            .and_then(|h| h.child_by_field_name("item"))
            .and_then(|i| i.utf8_text(source).ok())
            .map(|t| split_keyword(t.trim()).1)
            .unwrap_or_default();

        if pat.is_match(&title) {
            if path.len() == 1 {
                return Ok(Some(child));
            } else {
                return find_section_by_path(child, source, &path[1..]);
            }
        }
    }
    Ok(None)
}

/// Find the first section whose `:CUSTOM_ID:` property matches `id`.
fn find_section_by_custom_id<'a>(root: Node<'a>, source: &[u8], id: &str) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "section" {
            continue;
        }
        // check property_drawer for CUSTOM_ID
        if let Some(prop_drawer) = child.child_by_field_name("property_drawer") {
            let mut pc = prop_drawer.walk();
            for prop in prop_drawer.children(&mut pc) {
                if prop.kind() != "property" {
                    continue;
                }
                let name = prop
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                let value = prop
                    .child_by_field_name("value")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                if name.eq_ignore_ascii_case("CUSTOM_ID") && value.trim() == id {
                    return Some(child);
                }
            }
        }
        // recurse into subsections
        if let Some(found) = find_section_by_custom_id(child, source, id) {
            return Some(found);
        }
    }
    None
}

/// Return the raw org text of a subtree identified by a heading path or CUSTOM_ID.
///
/// Exactly one of `heading_path` or `custom_id` must be `Some`.
pub fn get_subtree(
    source: &[u8],
    tree: &Tree,
    heading_path: Option<&[String]>,
    custom_id: Option<&str>,
) -> Result<String> {
    let node = match (heading_path, custom_id) {
        (Some(path), None) => find_section_by_path(tree.root_node(), source, path)?,
        (None, Some(id)) => find_section_by_custom_id(tree.root_node(), source, id),
        _ => bail!("provide exactly one of heading_path or custom_id"),
    };
    match node {
        Some(n) => Ok(std::str::from_utf8(&source[n.start_byte()..n.end_byte()])
            .context("section text is not valid UTF-8")?
            .to_string()),
        None => bail!("section not found"),
    }
}
