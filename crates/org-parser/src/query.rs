use anyhow::{Context, Result, bail};
use regex::Regex;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::types::{ByteRange, HeadlineEntry, Position, QueryMatch};

// ── context helpers ───────────────────────────────────────────────────────────

fn header_line_numbers(source: &[u8]) -> Vec<usize> {
    std::str::from_utf8(source)
        .unwrap_or("")
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with('*'))
        .map(|(i, _)| i)
        .collect()
}

fn dist_from_prev_header(headers: &[usize], match_line: usize) -> usize {
    let idx = headers.partition_point(|&h| h < match_line);
    if idx == 0 { usize::MAX } else { match_line - headers[idx - 1] }
}

fn context_window(total: usize, dist: usize) -> (usize, usize) {
    let base: usize = match total {
        0..=5 => 4,
        6..=15 => 3,
        16..=35 => 2,
        36..=80 => 1,
        _ => 0,
    };
    let before = base.min(dist.saturating_sub(1));
    (before, base)
}

fn extract_context(lines: &[&str], start: usize, end: usize, before: usize, after: usize) -> Option<String> {
    if before == 0 && after == 0 {
        return None;
    }
    let from = start.saturating_sub(before);
    let to = (end + after + 1).min(lines.len());
    if from >= to {
        return None;
    }
    Some(lines[from..to].join("\n"))
}

// ── node helpers ──────────────────────────────────────────────────────────────

fn position(p: tree_sitter::Point) -> Position {
    Position { row: p.row, column: p.column }
}

fn byte_range(n: &Node) -> ByteRange {
    ByteRange { start: n.start_byte(), end: n.end_byte() }
}

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

fn split_keyword(title: &str) -> (Option<String>, String) {
    const KW: &[&str] = &["TODO", "DONE", "NEXT", "WAITING", "CANCELLED"];
    if let Some((first, rest)) = title.split_once(' ') {
        if KW.contains(&first) {
            return (Some(first.to_string()), rest.to_string());
        }
    }
    (None, title.to_string())
}

fn tags_from_headline(headline: Node, source: &[u8]) -> Vec<String> {
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

fn section_custom_id(section: Node, source: &[u8]) -> Option<String> {
    let pd = section.child_by_field_name("property_drawer")?;
    let mut c = pd.walk();
    for prop in pd.children(&mut c) {
        if prop.kind() != "property" {
            continue;
        }
        let name = prop.child_by_field_name("name")?.utf8_text(source).ok()?;
        if name.eq_ignore_ascii_case("CUSTOM_ID") {
            let val = prop.child_by_field_name("value")?.utf8_text(source).ok()?;
            return Some(val.trim().to_string());
        }
    }
    None
}

// ── query engine ──────────────────────────────────────────────────────────────

fn make_cursor(scm: &str) -> Result<(Query, QueryCursor)> {
    let language = tree_sitter_org::language();
    let query = Query::new(&language, scm).context("failed to compile query")?;
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(u32::MAX);
    Ok((query, cursor))
}

/// Execute `scm` rooted at `node`, invoking `f` with the first non-internal
/// capture of each match. Return `None` from `f` to skip a match.
fn query_nodes<'tree, T>(
    source: &[u8],
    node: Node<'tree>,
    scm: &str,
    mut f: impl FnMut(Node<'tree>, &[u8]) -> Option<T>,
) -> Result<Vec<T>> {
    let (query, mut cursor) = make_cursor(scm)?;
    let mut results = Vec::new();
    let mut matches = cursor.matches(&query, node, source);
    while let Some(qmatch) = matches.next() {
        for capture in qmatch.captures {
            let name = query.capture_names()[capture.index as usize];
            if !name.starts_with('_') {
                if let Some(val) = f(capture.node, source) {
                    results.push(val);
                }
                break;
            }
        }
    }
    if cursor.did_exceed_match_limit() {
        eprintln!("warn: query match limit exceeded — results may be incomplete");
    }
    Ok(results)
}

// ── outline ───────────────────────────────────────────────────────────────────

pub fn outline(source: &[u8], tree: &Tree) -> Result<Vec<HeadlineEntry>> {
    query_nodes(source, tree.root_node(), "(section) @section", |n, src| {
        let headline = n.child_by_field_name("headline")?;
        let stars = headline
            .child_by_field_name("stars")
            .and_then(|s| s.utf8_text(src).ok())
            .unwrap_or("");
        let item = headline.child_by_field_name("item")?;
        let full = item.utf8_text(src).ok()?.trim().to_string();
        let (keyword, title) = split_keyword(&full);
        Some(HeadlineEntry {
            depth: stars.len(),
            title,
            todo_keyword: keyword,
            tags: tags_from_headline(headline, src),
            range: byte_range(&n),
            start_position: position(headline.start_position()),
        })
    })
}

// ── run_query ─────────────────────────────────────────────────────────────────

/// Keep only results whose text matches at least one pattern, preferring the
/// structurally narrowest node (smallest byte range) for each regex hit span.
fn filter_by_patterns(results: Vec<QueryMatch>, patterns: &[Regex]) -> Vec<QueryMatch> {
    if patterns.is_empty() {
        return results;
    }

    // Absolute byte spans of every regex hit within each result's text.
    let result_spans: Vec<Vec<(usize, usize)>> = results
        .iter()
        .map(|qm| {
            let base = qm.range.start;
            patterns
                .iter()
                .flat_map(|pat| {
                    pat.find_iter(&qm.text)
                        .map(move |m| (base + m.start(), base + m.end()))
                })
                .collect()
        })
        .collect();

    // For each hit span, elect the result with the smallest enclosing byte range.
    let mut span_winner: std::collections::HashMap<(usize, usize), usize> =
        std::collections::HashMap::new();
    for (i, spans) in result_spans.iter().enumerate() {
        let size = results[i].range.end - results[i].range.start;
        for &span in spans {
            span_winner
                .entry(span)
                .and_modify(|w| {
                    let w_size = results[*w].range.end - results[*w].range.start;
                    if size < w_size {
                        *w = i;
                    }
                })
                .or_insert(i);
        }
    }

    let winners: std::collections::HashSet<usize> = span_winner.values().copied().collect();
    results
        .into_iter()
        .enumerate()
        .filter(|(i, _)| winners.contains(i))
        .map(|(_, qm)| qm)
        .collect()
}

/// Execute an arbitrary tree-sitter S-expression query.
/// Returns one [`QueryMatch`] per capture per pattern match, optionally
/// filtered to results whose text matches at least one of `patterns`.
pub fn run_query(source: &[u8], tree: &Tree, query_src: &str, patterns: &[Regex]) -> Result<Vec<QueryMatch>> {
    let (query, mut cursor) = make_cursor(query_src)?;
    let mut results = Vec::new();
    let mut match_id = 0usize;
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(qmatch) = matches.next() {
        for capture in qmatch.captures {
            let node = capture.node;
            let name = query.capture_names()[capture.index as usize].to_string();
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
                context: None,
            });
        }
        match_id += 1;
    }
    if cursor.did_exceed_match_limit() {
        eprintln!("warn: query match limit exceeded — results may be incomplete");
    }

    // Filter before context window so density heuristic reflects final count.
    let mut results = filter_by_patterns(results, patterns);

    let src_str = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = src_str.lines().collect();
    let headers = header_line_numbers(source);
    let total = results.len();
    for qm in &mut results {
        let start = qm.start_position.row;
        let end = qm.end_position.row;
        let dist = dist_from_prev_header(&headers, start);
        let (before, after) = context_window(total, dist);
        qm.context = extract_context(&lines, start, end, before, after);
    }

    Ok(results)
}

// ── get_subtree ───────────────────────────────────────────────────────────────

fn find_section_by_path<'tree>(
    source: &[u8],
    root: Node<'tree>,
    path: &[String],
) -> Result<Option<Node<'tree>>> {
    if path.is_empty() {
        return Ok(None);
    }
    let pat = Regex::new(&format!("(?i){}", path[0]))
        .with_context(|| format!("invalid heading regex: {:?}", path[0]))?;
    let root_id = root.id();
    let found = query_nodes(source, root, "(section) @section", |n, src| {
        if n.parent().map(|p| p.id()) != Some(root_id) {
            return None;
        }
        let title = n
            .child_by_field_name("headline")
            .and_then(|h| h.child_by_field_name("item"))
            .and_then(|i| i.utf8_text(src).ok())
            .map(|t| split_keyword(t.trim()).1)
            .unwrap_or_default();
        if pat.is_match(&title) { Some(n) } else { None }
    })?;
    match found.into_iter().next() {
        None => Ok(None),
        Some(child) if path.len() == 1 => Ok(Some(child)),
        Some(child) => find_section_by_path(source, child, &path[1..]),
    }
}

fn find_section_by_custom_id<'tree>(
    source: &[u8],
    root: Node<'tree>,
    id: &str,
) -> Result<Option<Node<'tree>>> {
    Ok(query_nodes(
        source, root,
        r#"(section (property_drawer (property
            name: (expr) @_name (#match? @_name "(?i)^CUSTOM_ID$")))) @section"#,
        |n, src| {
            section_custom_id(n, src).filter(|v| v == id).map(|_| n)
        },
    )?.into_iter().next())
}

pub fn get_subtree(
    source: &[u8],
    tree: &Tree,
    heading_path: Option<&[String]>,
    custom_id: Option<&str>,
) -> Result<String> {
    let node = match (heading_path, custom_id) {
        (Some(path), None) => find_section_by_path(source, tree.root_node(), path)?,
        (None, Some(id)) => find_section_by_custom_id(source, tree.root_node(), id)?,
        _ => bail!("provide exactly one of heading_path or custom_id"),
    };
    match node {
        Some(n) => Ok(std::str::from_utf8(&source[n.start_byte()..n.end_byte()])
            .context("section text is not valid UTF-8")?
            .to_string()),
        None => bail!("section not found"),
    }
}

// ── org link parsing ──────────────────────────────────────────────────────────

pub enum OrgLink {
    /// `[[#id]]` — CUSTOM_ID in the same file
    SameFileId(String),
    /// `[[file:path.org]]` — whole file, no section target
    File(String),
    /// `[[file:path.org::#id]]`
    FileId { file: String, id: String },
    /// `[[file:path.org::*Heading/Sub]]`
    FilePath { file: String, path: Vec<String> },
}

pub fn parse_org_link(raw: &str) -> Result<OrgLink> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .map(|s| s.splitn(2, "][").next().unwrap_or(s))
        .unwrap_or(trimmed);

    if let Some(id) = inner.strip_prefix('#') {
        return Ok(OrgLink::SameFileId(id.to_string()));
    }

    if let Some(rest) = inner.strip_prefix("file:") {
        return if let Some((path, target)) = rest.split_once("::") {
            if let Some(id) = target.strip_prefix('#') {
                Ok(OrgLink::FileId { file: path.to_string(), id: id.to_string() })
            } else if let Some(heading) = target.strip_prefix('*') {
                let parts = heading.split('/').map(|s| s.trim().to_string()).collect();
                Ok(OrgLink::FilePath { file: path.to_string(), path: parts })
            } else {
                bail!("unsupported link target syntax: {target:?}")
            }
        } else {
            Ok(OrgLink::File(rest.to_string()))
        };
    }

    bail!("unsupported link format: {raw:?}")
}
