use anyhow::{Context, Result, bail};
use regex::Regex;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::types::{ByteRange, HeadlineEntry, Position, QueryMatch, SectionInfo};

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

// ── section_for / get_subtree ─────────────────────────────────────────────────

/// Return structured metadata for a section matched by all provided criteria
/// (AND-ed). At least one of `heading_path`, `custom_id`, or `line` must be
/// supplied. When multiple sections match, the innermost (smallest byte span)
/// wins — so adding `line` to a `heading_path` that matches duplicate titles
/// resolves the ambiguity without extra round-trips.
pub fn section_for(
    source: &[u8],
    tree: &Tree,
    heading_path: Option<&[String]>,
    custom_id: Option<&str>,
    line: Option<usize>,
) -> Result<Option<SectionInfo>> {
    if heading_path.is_none() && custom_id.is_none() && line.is_none() {
        bail!("provide at least one of heading_path, custom_id, or line");
    }

    // Pre-compile heading path patterns once outside the hot loop.
    let path_patterns: Option<Vec<Regex>> = heading_path
        .map(|path| {
            path.iter()
                .map(|p| {
                    Regex::new(&format!("(?i){p}"))
                        .map_err(|e| anyhow::anyhow!("invalid regex {p:?}: {e}"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let hits = query_nodes(source, tree.root_node(), "(section) @section", |n, src| {
        // heading_path: breadcrumbs + own title must each match the corresponding pattern
        if let Some(ref patterns) = path_patterns {
            let hl = n.child_by_field_name("headline")?;
            let title = hl
                .child_by_field_name("item")
                .and_then(|i| i.utf8_text(src).ok())
                .map(|t| split_keyword(t.trim()).1)
                .unwrap_or_default();
            let crumbs = breadcrumbs(n, src);
            if crumbs.len() + 1 != patterns.len() {
                return None;
            }
            let all_match = crumbs
                .iter()
                .chain(std::iter::once(&title))
                .zip(patterns.iter())
                .all(|(text, re)| re.is_match(text));
            if !all_match {
                return None;
            }
        }

        // custom_id: section must carry exactly this CUSTOM_ID
        if let Some(id) = custom_id {
            if section_custom_id(n, src).as_deref() != Some(id) {
                return None;
            }
        }

        // line: section must span this 0-indexed row
        if let Some(ln) = line {
            let start = n.start_position();
            let end = n.end_position();
            let contains =
                start.row <= ln && (ln < end.row || (ln == end.row && end.column > 0));
            if !contains {
                return None;
            }
        }

        Some(n)
    })?;

    // Nested hits (parent + child both spanning the same line) are expected:
    // pick the innermost. Sibling hits mean the criteria are ambiguous → error.
    let innermost = match hits.iter().min_by_key(|n| n.end_byte() - n.start_byte()) {
        Some(n) => *n,
        None => return Ok(None),
    };
    for hit in &hits {
        if hit.id() == innermost.id() {
            continue;
        }
        // Every non-innermost hit must be an ancestor of the innermost.
        let mut cur = innermost.parent();
        let is_ancestor = loop {
            match cur {
                Some(p) if p.id() == hit.id() => break true,
                Some(p) => cur = p.parent(),
                None => break false,
            }
        };
        if !is_ancestor {
            bail!(
                "ambiguous: {} sections matched the given criteria; \
                 add `line` or `custom_id` to disambiguate",
                hits.len()
            );
        }
    }
    let section = innermost;

    let headline = section
        .child_by_field_name("headline")
        .ok_or_else(|| anyhow::anyhow!("section has no headline"))?;
    let stars = headline
        .child_by_field_name("stars")
        .and_then(|s| s.utf8_text(source).ok())
        .unwrap_or("");
    let full_item = headline
        .child_by_field_name("item")
        .and_then(|i| i.utf8_text(source).ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let (todo_keyword, title) = split_keyword(&full_item);

    Ok(Some(SectionInfo {
        title,
        depth: stars.len(),
        todo_keyword,
        custom_id: section_custom_id(section, source),
        breadcrumbs: breadcrumbs(section, source),
        start_line: headline.start_position().row,
        range: byte_range(&section),
        subtree: std::str::from_utf8(&source[section.start_byte()..section.end_byte()])
            .context("section text is not valid UTF-8")?
            .to_string(),
    }))
}

/// Return the full org text of a section. Used internally by `open_link`.
pub fn get_subtree(
    source: &[u8],
    tree: &Tree,
    heading_path: Option<&[String]>,
    custom_id: Option<&str>,
) -> Result<String> {
    section_for(source, tree, heading_path, custom_id, None)?
        .map(|info| info.subtree)
        .ok_or_else(|| anyhow::anyhow!("section not found"))
}

// ── patch_subtree ─────────────────────────────────────────────────────────────

/// Apply a literal search-and-replace within the subtree identified by
/// `custom_id`. Returns `(modified_file_bytes, new_section_text)`.
pub fn patch_subtree(
    source: &[u8],
    tree: &Tree,
    custom_id: &str,
    search: &str,
    replace: &str,
) -> Result<(Vec<u8>, String)> {
    let info = section_for(source, tree, None, Some(custom_id), None)?
        .ok_or_else(|| anyhow::anyhow!("section with CUSTOM_ID {:?} not found", custom_id))?;

    if !info.subtree.contains(search) {
        bail!("search string not found in section {:?}", custom_id);
    }

    let new_section = info.subtree.replace(search, replace);
    let mut modified = Vec::with_capacity(
        source.len() + new_section.len().saturating_sub(info.subtree.len()),
    );
    modified.extend_from_slice(&source[..info.range.start]);
    modified.extend_from_slice(new_section.as_bytes());
    modified.extend_from_slice(&source[info.range.end..]);

    Ok((modified, new_section))
}

// ── nodes_at_line / ensure_custom_id ─────────────────────────────────────────

/// Return all nodes matched by `scm` that span `line` (0-indexed row),
/// picking the innermost (smallest byte span) when there are overlaps.
/// Composable primitive; expose as an MCP tool if agents ever need to locate
/// arbitrary node types (blocks, list items, …) by line.
fn nodes_at_line<'tree>(
    source: &[u8],
    root: Node<'tree>,
    scm: &str,
    line: usize,
) -> Result<Vec<Node<'tree>>> {
    query_nodes(source, root, scm, |n, _| {
        let start = n.start_position();
        let end = n.end_position();
        // end_position is an exclusive cursor: (row=R, col=0) means the node
        // ends just before row R, so row R is NOT part of this node.
        let contains = start.row <= line
            && (line < end.row || (line == end.row && end.column > 0));
        if contains { Some(n) } else { None }
    })
}

fn find_section_containing_line<'tree>(
    source: &[u8],
    root: Node<'tree>,
    line: usize,
) -> Result<Option<Node<'tree>>> {
    let hits = nodes_at_line(source, root, "(section) @section", line)?;
    Ok(hits.into_iter().min_by_key(|n| n.end_byte() - n.start_byte()))
}

fn collect_custom_ids(source: &[u8], root: Node) -> Result<Vec<String>> {
    query_nodes(
        source,
        root,
        r#"(section (property_drawer (property
            name: (expr) @_name (#match? @_name "(?i)^CUSTOM_ID$")))) @section"#,
        |n, src| section_custom_id(n, src),
    )
}

fn insert_custom_id_into_section(source: &[u8], section: Node, id: &str) -> Result<Vec<u8>> {
    let existing_pd = section.child_by_field_name("property_drawer");

    let (insert_pos, to_insert) = if let Some(pd) = existing_pd {
        let pd_text = pd
            .utf8_text(source)
            .context("property drawer is not valid UTF-8")?;
        let end_offset = pd_text
            .find(":END:")
            .ok_or_else(|| anyhow::anyhow!("property drawer missing :END:"))?;
        (
            pd.start_byte() + end_offset,
            format!(":CUSTOM_ID: {id}\n"),
        )
    } else {
        let headline = section
            .child_by_field_name("headline")
            .ok_or_else(|| anyhow::anyhow!("section has no headline"))?;
        let mut c = section.walk();
        let pos = section
            .children(&mut c)
            .find(|n| n.kind() == "plan")
            .map(|n| n.end_byte())
            .unwrap_or_else(|| headline.end_byte());
        (pos, format!(":PROPERTIES:\n:CUSTOM_ID: {id}\n:END:\n"))
    };

    let mut result = Vec::with_capacity(source.len() + to_insert.len());
    result.extend_from_slice(&source[..insert_pos]);
    result.extend_from_slice(to_insert.as_bytes());
    result.extend_from_slice(&source[insert_pos..]);
    Ok(result)
}

pub struct EnsureCustomIdResult {
    pub custom_id: String,
    pub subtree: String,
    pub file_content: Vec<u8>,
    pub already_existed: bool,
}

/// Ensure the section spanning `line` (0-indexed row, as reported by
/// `outline` / `query` `start_position.row`) has a `:CUSTOM_ID:` property.
/// If one already exists it is returned unchanged. Otherwise `proposed_id` is
/// checked for uniqueness across the file; a `-2`, `-3`, … suffix is appended
/// if needed. Returns the assigned ID, the updated subtree text, the full
/// modified file bytes, and whether the ID pre-existed.
pub fn ensure_custom_id(
    source: &[u8],
    tree: &Tree,
    line: usize,
    proposed_id: &str,
) -> Result<EnsureCustomIdResult> {
    let root = tree.root_node();

    let section = find_section_containing_line(source, root, line)?
        .ok_or_else(|| anyhow::anyhow!("no section found at line {line}"))?;

    if let Some(existing_id) = section_custom_id(section, source) {
        let subtree =
            std::str::from_utf8(&source[section.start_byte()..section.end_byte()])
                .context("section text is not valid UTF-8")?
                .to_string();
        return Ok(EnsureCustomIdResult {
            custom_id: existing_id,
            subtree,
            file_content: source.to_vec(),
            already_existed: true,
        });
    }

    let existing_ids = collect_custom_ids(source, root)?;
    let final_id = if !existing_ids.iter().any(|id| id == proposed_id) {
        proposed_id.to_string()
    } else {
        let mut i = 2u32;
        loop {
            let candidate = format!("{proposed_id}-{i}");
            if !existing_ids.iter().any(|id| id == &candidate) {
                break candidate;
            }
            i += 1;
        }
    };

    let file_content = insert_custom_id_into_section(source, section, &final_id)?;

    let mut parser = crate::parser::make_parser()?;
    let new_tree = parser
        .parse(&file_content, None)
        .ok_or_else(|| anyhow::anyhow!("failed to re-parse after CUSTOM_ID insertion"))?;
    let subtree = get_subtree(&file_content, &new_tree, None, Some(&final_id))?;

    Ok(EnsureCustomIdResult {
        custom_id: final_id,
        subtree,
        file_content,
        already_existed: false,
    })
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

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::make_parser;

    fn parse(src: &[u8]) -> tree_sitter::Tree {
        let mut p = make_parser().unwrap();
        p.parse(src, None).unwrap()
    }

    const ORG: &str = "\
* Alpha
:PROPERTIES:
:CUSTOM_ID: alpha
:END:
Content of alpha.

* Beta
Content of beta without custom id.

** Beta Sub
:PROPERTIES:
:CUSTOM_ID: beta-sub
:END:
Sub-section content.
";

    #[test]
    fn patch_replaces_text_in_section() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let (new_bytes, new_text) =
            patch_subtree(src, &tree, "alpha", "Content of alpha", "Updated content").unwrap();
        assert!(new_text.contains("Updated content"));
        assert!(!new_text.contains("Content of alpha"));
        // Sections outside alpha are unchanged.
        let rest = std::str::from_utf8(&new_bytes).unwrap();
        assert!(rest.contains("Content of beta without custom id"));
    }

    #[test]
    fn patch_errors_on_missing_id() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let err = patch_subtree(src, &tree, "nonexistent", "x", "y").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn patch_errors_when_search_not_found() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let err = patch_subtree(src, &tree, "alpha", "no such text", "y").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ORG line layout (0-indexed rows):
    //  0: * Alpha
    //  1: :PROPERTIES:
    //  2: :CUSTOM_ID: alpha
    //  3: :END:
    //  4: Content of alpha.
    //  5: (blank)
    //  6: * Beta
    //  7: Content of beta without custom id.
    //  8: (blank)
    //  9: ** Beta Sub
    // 10: :PROPERTIES:
    // 11: :CUSTOM_ID: beta-sub
    // 12: :END:
    // 13: Sub-section content.

    #[test]
    fn section_for_by_line() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let info = section_for(src, &tree, None, None, Some(4)).unwrap().unwrap();
        assert_eq!(info.title, "Alpha");
        assert_eq!(info.depth, 1);
        assert_eq!(info.custom_id.as_deref(), Some("alpha"));
        assert_eq!(info.start_line, 0);
        assert!(info.breadcrumbs.is_empty());
    }

    #[test]
    fn section_for_by_custom_id() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let info = section_for(src, &tree, None, Some("beta-sub"), None).unwrap().unwrap();
        assert_eq!(info.title, "Beta Sub");
        assert_eq!(info.breadcrumbs, vec!["Beta"]);
    }

    #[test]
    fn section_for_by_heading_path() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let path = vec!["Beta".to_string(), "Beta Sub".to_string()];
        let info = section_for(src, &tree, Some(&path), None, None).unwrap().unwrap();
        assert_eq!(info.title, "Beta Sub");
    }

    #[test]
    fn section_for_heading_path_and_line_combined() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // heading_path matches "Beta" AND line is inside Beta (not Beta Sub) — innermost wins
        let path = vec!["Beta".to_string()];
        let info = section_for(src, &tree, Some(&path), None, Some(7)).unwrap().unwrap();
        assert_eq!(info.title, "Beta");
    }

    #[test]
    fn section_for_picks_innermost_by_line() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 13 is inside Beta Sub (depth 2), not Beta (depth 1).
        // Both sections span row 13, but Beta Sub is nested inside Beta →
        // valid nesting, pick the innermost.
        let info = section_for(src, &tree, None, None, Some(13)).unwrap().unwrap();
        assert_eq!(info.title, "Beta Sub");
        assert_eq!(info.breadcrumbs, vec!["Beta"]);
    }

    #[test]
    fn section_for_errors_on_ambiguous_siblings() {
        // Two sibling sections with the same heading "Results".
        let src = b"\
* Results
Content A.

* Results
Content B.
";
        let tree = parse(src);
        let path = vec!["Results".to_string()];
        let err = section_for(src, &tree, Some(&path), None, None).unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn section_for_disambiguates_siblings_with_line() {
        let src = b"\
* Results
Content A.

* Results
Content B.
";
        let tree = parse(src);
        let path = vec!["Results".to_string()];
        // Row 1 is inside the first "Results" section.
        let info = section_for(src, &tree, Some(&path), None, Some(1)).unwrap().unwrap();
        assert_eq!(info.start_line, 0);
        // Row 4 is inside the second "Results" section.
        let info2 = section_for(src, &tree, Some(&path), None, Some(4)).unwrap().unwrap();
        assert_eq!(info2.start_line, 3);
    }

    #[test]
    fn ensure_custom_id_detects_existing() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 0 is the Alpha headline which already has CUSTOM_ID "alpha".
        let res = ensure_custom_id(src, &tree, 0, "anything").unwrap();
        assert!(res.already_existed);
        assert_eq!(res.custom_id, "alpha");
    }

    #[test]
    fn ensure_custom_id_inserts_new() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 7 is content inside Beta, which has no CUSTOM_ID.
        let res = ensure_custom_id(src, &tree, 7, "beta").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "beta");
        assert!(res.subtree.contains(":CUSTOM_ID: beta"));
    }

    #[test]
    fn ensure_custom_id_disambiguates_collision() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // "alpha" is already taken; Beta should get "alpha-2".
        let res = ensure_custom_id(src, &tree, 6, "alpha").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "alpha-2");
    }

    #[test]
    fn ensure_custom_id_by_headline_row() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 6 is the Beta headline itself — still resolves to the Beta section.
        let res = ensure_custom_id(src, &tree, 6, "beta-new").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "beta-new");
    }
}
