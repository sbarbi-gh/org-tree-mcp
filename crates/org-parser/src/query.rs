use anyhow::{Context, Result, bail};
use regex::Regex;
use std::path::Path;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::types::{
    ByteRange, Dest, Diagnostic, DiagnosticKind, HeadlineEntry, Position, QueryMatch,
    RefileOutput, SectionInfo, SectionRef, ValidationReport,
};


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

/// Keep only results whose text matches all patterns (AND), preferring the
/// structurally narrowest node (smallest byte range) for each regex hit span.
fn filter_by_patterns(results: Vec<QueryMatch>, patterns: &[Regex]) -> Vec<QueryMatch> {
    if patterns.is_empty() {
        return results;
    }

    // For each result, collect all hit spans only if every pattern matches at least once.
    let result_spans: Vec<Option<Vec<(usize, usize)>>> = results
        .iter()
        .map(|qm| {
            let base = qm.range.start;
            let mut all_spans = Vec::new();
            for pat in patterns {
                let spans: Vec<_> = pat
                    .find_iter(&qm.text)
                    .map(|m| (base + m.start(), base + m.end()))
                    .collect();
                if spans.is_empty() {
                    return None; // pattern not found — AND fails
                }
                all_spans.extend(spans);
            }
            Some(all_spans)
        })
        .collect();

    // For each hit span, elect the result with the smallest enclosing byte range.
    let mut span_winner: std::collections::HashMap<(usize, usize), usize> =
        std::collections::HashMap::new();
    for (i, spans_opt) in result_spans.iter().enumerate() {
        if let Some(spans) = spans_opt {
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
/// filtered to results whose text matches all of `patterns` (AND).
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

// ── section_for ───────────────────────────────────────────────────────────────

/// Convert a matched section `Node` into a `SectionInfo`.
fn node_to_section_info(source: &[u8], section: Node) -> Result<SectionInfo> {
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
    Ok(SectionInfo {
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
    })
}

/// Locate a section by a [`SectionRef`] criterion. Returns `None` when no
/// section matches; returns an error when the criterion is ambiguous (path
/// matching two sibling sections with the same title — use `Line` to
/// disambiguate).
pub fn section_for(source: &[u8], tree: &Tree, r: &SectionRef) -> Result<Option<SectionInfo>> {
    match r {
        SectionRef::Id { id, .. } => {
            let hit = query_nodes(source, tree.root_node(), "(section) @section", |n, src| {
                if section_custom_id(n, src).as_deref() == Some(id.as_str()) { Some(n) } else { None }
            })?
            .into_iter()
            .next();
            hit.map(|n| node_to_section_info(source, n)).transpose()
        }

        SectionRef::Line { line, .. } => {
            find_section_containing_line(source, tree.root_node(), *line)?
                .map(|n| node_to_section_info(source, n))
                .transpose()
        }

        SectionRef::Path { path, .. } => {
            let patterns: Vec<Regex> = path
                .iter()
                .map(|p| {
                    Regex::new(&format!("(?i){p}"))
                        .map_err(|e| anyhow::anyhow!("invalid regex {p:?}: {e}"))
                })
                .collect::<Result<_>>()?;

            let hits = query_nodes(source, tree.root_node(), "(section) @section", |n, src| {
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
                if all_match { Some(n) } else { None }
            })?;

            // Nested hits are expected (parent and child can share a title prefix);
            // pick the innermost. Sibling hits are ambiguous → error.
            let innermost = match hits.iter().min_by_key(|n| n.end_byte() - n.start_byte()) {
                Some(n) => *n,
                None => return Ok(None),
            };
            for hit in &hits {
                if hit.id() == innermost.id() {
                    continue;
                }
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
                        "ambiguous: {} sections matched path {:?}; use `line` to disambiguate",
                        hits.len(),
                        path
                    );
                }
            }
            node_to_section_info(source, innermost).map(Some)
        }
    }
}

// ── patch_subtree ─────────────────────────────────────────────────────────────

/// Apply a literal search-and-replace within the subtree identified by `r`.
/// Returns `(modified_file_bytes, new_section_text)`.
pub fn patch_subtree(
    source: &[u8],
    tree: &Tree,
    r: &SectionRef,
    search: &str,
    replace: &str,
) -> Result<(Vec<u8>, String)> {
    let info = resolve_section_ref(source, tree, r)?;

    if !info.subtree.contains(search) {
        bail!("search string not found in section {:?}", info.title);
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

/// Ensure the section identified by `r` has a `:CUSTOM_ID:` property.
/// If one already exists it is returned unchanged. Otherwise `proposed_id` is
/// checked for uniqueness across the file; a `-2`, `-3`, … suffix is appended
/// if needed. Returns the assigned ID, the updated subtree text, the full
/// modified file bytes, and whether the ID pre-existed.
pub fn ensure_custom_id(
    source: &[u8],
    tree: &Tree,
    r: &SectionRef,
    proposed_id: &str,
) -> Result<EnsureCustomIdResult> {
    let root = tree.root_node();

    let info = resolve_section_ref(source, tree, r)?;

    if let Some(existing_id) = &info.custom_id {
        return Ok(EnsureCustomIdResult {
            custom_id: existing_id.clone(),
            subtree: info.subtree,
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

    let section = find_section_containing_line(source, root, info.start_line)?
        .ok_or_else(|| anyhow::anyhow!("internal: section at line {} not found", info.start_line))?;

    let file_content = insert_custom_id_into_section(source, section, &final_id)?;

    let mut parser = crate::parser::make_parser()?;
    let new_tree = parser
        .parse(&file_content, None)
        .ok_or_else(|| anyhow::anyhow!("failed to re-parse after CUSTOM_ID insertion"))?;
    let subtree = resolve_section_ref(
        &file_content, &new_tree, &SectionRef::Id { file: None, id: final_id.clone() },
    )?.subtree;

    Ok(EnsureCustomIdResult {
        custom_id: final_id,
        subtree,
        file_content,
        already_existed: false,
    })
}

// ── resolve_section_ref ───────────────────────────────────────────────────────

/// `section_for` variant that treats a missing section as an error rather than
/// `None`. Use this when the caller requires exactly one result.
pub fn resolve_section_ref(source: &[u8], tree: &Tree, r: &SectionRef) -> Result<SectionInfo> {
    section_for(source, tree, r)?.ok_or_else(|| match r {
        SectionRef::Id   { id,   .. } => anyhow::anyhow!("no section with CUSTOM_ID {:?}", id),
        SectionRef::Line { line, .. } => anyhow::anyhow!("no section at line {line}"),
        SectionRef::Path { path, .. } => anyhow::anyhow!("no section at path {:?}", path),
    })
}

// ── validation ────────────────────────────────────────────────────────────────

/// Walk the AST and collect ERROR / MISSING nodes as fatal diagnostics.
fn collect_error_nodes(node: Node, source: &[u8], errors: &mut Vec<Diagnostic>) {
    if node.is_error() {
        let pos = node.start_position();
        let snippet: String = node.utf8_text(source).unwrap_or("").chars().take(60).collect();
        errors.push(Diagnostic {
            kind: DiagnosticKind::ParseError,
            line: Some(pos.row),
            message: format!("syntax error near {:?}", snippet),
        });
        return; // don't recurse into the error subtree
    }
    if node.is_missing() {
        let pos = node.start_position();
        errors.push(Diagnostic {
            kind: DiagnosticKind::ParseError,
            line: Some(pos.row),
            message: format!("missing `{}`", node.kind()),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_error_nodes(child, source, errors);
    }
}

/// Return `(custom_id, headline_row)` for every section in the document that
/// carries a `:CUSTOM_ID:` property.
fn collect_custom_ids_with_lines(source: &[u8], root: Node) -> Result<Vec<(String, usize)>> {
    query_nodes(
        source,
        root,
        r#"(section (property_drawer (property
            name: (expr) @_name (#match? @_name "(?i)^CUSTOM_ID$")))) @section"#,
        |n, src| {
            let id = section_custom_id(n, src)?;
            let line = n.child_by_field_name("headline")?.start_position().row;
            Some((id, line))
        },
    )
}

/// Validate an already-parsed org buffer.  Runs all checks (parse errors,
/// duplicate CUSTOM_IDs) and returns a [`ValidationReport`].
/// Callers that need to parse first should use the public `validate()` entry
/// point instead.
fn validate_tree(source: &[u8], tree: &Tree) -> Result<ValidationReport> {
    let mut errors = Vec::new();
    let warnings = Vec::new();

    collect_error_nodes(tree.root_node(), source, &mut errors);

    let all_ids = collect_custom_ids_with_lines(source, tree.root_node())?;
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (id, line) in all_ids {
        if let Some(first_line) = seen.get(&id) {
            errors.push(Diagnostic {
                kind: DiagnosticKind::DuplicateCustomId,
                line: Some(line),
                message: format!("CUSTOM_ID {:?} already defined at line {}", id, first_line + 1),
            });
        } else {
            seen.insert(id, line);
        }
    }

    Ok(ValidationReport { errors, warnings })
}

/// Parse `source` with tree-sitter and return a full [`ValidationReport`].
/// `errors` are fatal (write should be aborted); `warnings` are informational.
pub fn validate(source: &[u8]) -> Result<ValidationReport> {
    let mut parser = crate::parser::make_parser()?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse for validation"))?;
    validate_tree(source, &tree)
}

// ── refile helpers ────────────────────────────────────────────────────────────

/// Re-star all headlines in `subtree` by `delta` levels (positive = deeper,
/// negative = shallower). The minimum star count is 1.  Uses a fresh
/// tree-sitter parse of the subtree so that stars inside code blocks are
/// never touched.
fn adjust_depth(subtree: &str, delta: i32) -> Result<String> {
    if delta == 0 {
        return Ok(subtree.to_string());
    }
    let mut parser = crate::parser::make_parser()?;
    let src = subtree.as_bytes();
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse subtree for depth adjustment"))?;

    // Collect byte ranges of all star nodes in document order.
    let star_ranges: Vec<(usize, usize)> = query_nodes(
        src,
        tree.root_node(),
        "(section headline: (headline stars: (stars) @stars))",
        |n, _| Some((n.start_byte(), n.end_byte())),
    )?;

    // Apply in reverse so earlier byte offsets stay valid.
    let mut result = src.to_vec();
    for (start, end) in star_ranges.into_iter().rev() {
        let current = end - start;
        let new_count = ((current as i32 + delta).max(1)) as usize;
        let new_stars: Vec<u8> = std::iter::repeat(b'*').take(new_count).collect();
        result.splice(start..end, new_stars);
    }

    String::from_utf8(result)
        .map_err(|e| anyhow::anyhow!("non-UTF8 after depth adjustment: {e}"))
}

/// Limit consecutive `\n` to at most 2 (= one blank line between sections).
/// Applied to small windows around insertion/deletion sites.
fn collapse_blank_lines(bytes: Vec<u8>) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut nl_run = 0usize;
    for b in bytes {
        if b == b'\n' {
            nl_run += 1;
            if nl_run <= 2 {
                result.push(b);
            }
        } else {
            nl_run = 0;
            result.push(b);
        }
    }
    result
}

/// Return `""`, `"\n"`, or `"\n\n"` — the prefix needed to ensure exactly
/// one blank line separates whatever is at `buf[..offset]` from the next
/// inserted section.  Returns `""` at offset 0 (file start).
fn blank_line_prefix(buf: &[u8], offset: usize) -> &'static str {
    if offset == 0 {
        return "";
    }
    if buf[..offset].ends_with(b"\n\n") { "" }
    else if buf[..offset].ends_with(b"\n") { "\n" }
    else { "\n\n" }
}

/// Insert `content` at `offset` in `dest`, ensuring a blank-line separator
/// before the inserted block and collapsing any resulting excess blank lines.
fn insert_with_padding(dest: &[u8], offset: usize, content: &str) -> Vec<u8> {
    let prefix = blank_line_prefix(dest, offset);
    let mut result = Vec::with_capacity(dest.len() + prefix.len() + content.len());
    result.extend_from_slice(&dest[..offset]);
    result.extend_from_slice(prefix.as_bytes());
    result.extend_from_slice(content.as_bytes());
    result.extend_from_slice(&dest[offset..]);
    collapse_blank_lines(result)
}

/// Remove bytes `[start..end]` from `source` and collapse excess blank lines
/// in the surrounding context.
fn remove_section_bytes(source: &[u8], start: usize, end: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(source.len().saturating_sub(end - start));
    result.extend_from_slice(&source[..start]);
    result.extend_from_slice(&source[end..]);
    collapse_blank_lines(result)
}

/// Byte offset of the first top-level section in the document, or `None` if
/// the document has no sections (e.g., only front-matter directives).
fn first_top_level_section_start(source: &[u8], tree: &Tree) -> Result<Option<usize>> {
    let starts = query_nodes(source, tree.root_node(), "(section) @s", |n, _| {
        // A top-level section's immediate parent is the document, not a section.
        if n.parent().map(|p| p.kind()) != Some("section") {
            Some(n.start_byte())
        } else {
            None
        }
    })?;
    Ok(starts.into_iter().min())
}

/// Byte offset of the first direct child section of `dest` (depth + 1),
/// or `dest.range.end` if `dest` has no children.
fn first_direct_child_start(source: &[u8], tree: &Tree, dest: &SectionInfo) -> Result<usize> {
    let child_starts = query_nodes(source, tree.root_node(), "(section) @s", |n, src| {
        // Must be strictly inside dest's byte range.
        if n.start_byte() <= dest.range.start || n.end_byte() > dest.range.end {
            return None;
        }
        // Must be a direct child: stars count = dest.depth + 1.
        let hl = n.child_by_field_name("headline")?;
        let stars = hl.child_by_field_name("stars")?.utf8_text(src).ok()?;
        if stars.len() == dest.depth + 1 { Some(n.start_byte()) } else { None }
    })?;
    Ok(child_starts.into_iter().min().unwrap_or(dest.range.end))
}

/// Compute the raw insertion byte offset within `dest_source` for the given
/// placement.
fn insertion_offset(
    dest_source: &[u8],
    dest_tree: &Tree,
    dest_info: Option<&SectionInfo>,
    dest: &Dest,
) -> Result<usize> {
    match dest {
        Dest::DocTop { .. } => Ok(
            first_top_level_section_start(dest_source, dest_tree)?
                .unwrap_or(dest_source.len()),
        ),
        Dest::DocBottom { .. }  => Ok(dest_source.len()),
        Dest::Before { .. }    => Ok(dest_info.unwrap().range.start),
        Dest::After { .. }     => Ok(dest_info.unwrap().range.end),
        Dest::LastChild { .. } => Ok(dest_info.unwrap().range.end),
        Dest::FirstChild { .. } => {
            first_direct_child_start(dest_source, dest_tree, dest_info.unwrap())
        }
    }
}

/// Produce modified bytes for a same-file refile (insert + remove in one pass).
fn build_same_file(
    source: &[u8],
    src_start: usize,
    src_end: usize,
    insert_at: usize,
    content: &str,
) -> Result<Vec<u8>> {
    if insert_at > src_start && insert_at <= src_end {
        bail!("circular refile: insertion point is inside the source section");
    }

    let combined = if insert_at <= src_start {
        // Insert first (before src), then remove the shifted src range.
        let prefix = blank_line_prefix(source, insert_at);
        let shift = prefix.len() + content.len();
        let new_src_start = src_start + shift;
        let new_src_end   = src_end   + shift;

        let mut inter = Vec::with_capacity(source.len() + shift);
        inter.extend_from_slice(&source[..insert_at]);
        inter.extend_from_slice(prefix.as_bytes());
        inter.extend_from_slice(content.as_bytes());
        inter.extend_from_slice(&source[insert_at..]);

        let mut r = Vec::with_capacity(inter.len() - (src_end - src_start));
        r.extend_from_slice(&inter[..new_src_start]);
        r.extend_from_slice(&inter[new_src_end..]);
        collapse_blank_lines(r)
    } else {
        // insert_at > src_end: remove src first, then insert at adjusted offset.
        let removed = src_end - src_start;
        let new_insert = insert_at - removed;

        let mut inter = Vec::with_capacity(source.len() - removed);
        inter.extend_from_slice(&source[..src_start]);
        inter.extend_from_slice(&source[src_end..]);

        let prefix = blank_line_prefix(&inter, new_insert);
        let mut r = Vec::with_capacity(inter.len() + prefix.len() + content.len());
        r.extend_from_slice(&inter[..new_insert]);
        r.extend_from_slice(prefix.as_bytes());
        r.extend_from_slice(content.as_bytes());
        r.extend_from_slice(&inter[new_insert..]);
        collapse_blank_lines(r)
    };
    Ok(combined)
}

// ── refile_subtree ────────────────────────────────────────────────────────────

/// Core refile algorithm. Both `src_info` and `dest_info` must already be
/// resolved by the caller. `same_file` must be true when `src_source` and
/// `dest_source` refer to the same file.
pub(crate) fn refile_bytes(
    src_source: &[u8],
    src_file: &str,
    src_info: &SectionInfo,
    dest_source: &[u8],
    dest_file: &str,
    dest_tree: &Tree,
    dest_info: Option<&SectionInfo>,
    dest: &Dest,
    same_file: bool,
) -> Result<RefileOutput> {
    // ── 1. Require CUSTOM_ID on source ────────────────────────────────────────
    let src_custom_id = src_info.custom_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "source section {:?} has no :CUSTOM_ID: — call ensure_custom_id first",
            src_info.title
        )
    })?;

    // ── 2. Compute target depth ───────────────────────────────────────────────
    let new_depth: usize = match dest {
        Dest::DocTop { .. } | Dest::DocBottom { .. } => 1,
        Dest::Before { .. } | Dest::After { .. } => {
            dest_info.map(|d| d.depth).unwrap_or(1)
        }
        Dest::FirstChild { .. } | Dest::LastChild { .. } => {
            dest_info.map(|d| d.depth + 1).unwrap_or(1)
        }
    };

    // ── 3. Circularity guard (same file only) ─────────────────────────────────
    if same_file {
        if let Some(di) = dest_info {
            if di.range.start > src_info.range.start && di.range.end <= src_info.range.end {
                bail!("circular refile: destination is inside the source section");
            }
            if di.range.start == src_info.range.start {
                bail!("cannot refile a section relative to itself");
            }
        }
    }

    // ── 4. Depth-adjust the extracted subtree ─────────────────────────────────
    let delta = new_depth as i32 - src_info.depth as i32;
    let mut adjusted = adjust_depth(&src_info.subtree, delta)?;
    if !adjusted.ends_with('\n') {
        adjusted.push('\n');
    }

    // ── 5. Disambiguate CUSTOM_ID in dest ────────────────────────────────────
    let existing_dest_ids = collect_custom_ids(dest_source, dest_tree.root_node())?;
    // For same-file refiling the src section is removed in the same pass, so
    // its own ID is not a collision — only count it as taken if another section
    // already carries the same ID (count > 1).
    let id_occurrences = existing_dest_ids.iter().filter(|id| id.as_str() == src_custom_id).count();
    let has_collision = if same_file { id_occurrences > 1 } else { id_occurrences > 0 };
    let final_custom_id = if has_collision {
        (2u32..)
            .find_map(|i| {
                let c = format!("{src_custom_id}-{i}");
                if !existing_dest_ids.iter().any(|id| id == &c) { Some(c) } else { None }
            })
            .unwrap() // infinite iterator, always terminates
    } else {
        src_custom_id.to_string()
    };
    let custom_id_changed = final_custom_id != src_custom_id;

    if custom_id_changed {
        adjusted = adjusted.replace(
            &format!(":CUSTOM_ID: {src_custom_id}"),
            &format!(":CUSTOM_ID: {final_custom_id}"),
        );
    }

    // ── 6. Compute insertion offset ───────────────────────────────────────────
    let insert_at = insertion_offset(dest_source, dest_tree, dest_info, dest)?;

    // ── 7. Build modified byte buffers ────────────────────────────────────────
    let (src_bytes, dest_bytes) = if same_file {
        let combined = build_same_file(
            src_source,
            src_info.range.start,
            src_info.range.end,
            insert_at,
            &adjusted,
        )?;
        (combined.clone(), combined)
    } else {
        let d = insert_with_padding(dest_source, insert_at, &adjusted);
        let s = remove_section_bytes(src_source, src_info.range.start, src_info.range.end);
        (s, d)
    };

    // ── 8. Re-parse dest to get final line number + validate ──────────────────
    let mut parser = crate::parser::make_parser()?;
    let dest_tree_new = parser
        .parse(&dest_bytes, None)
        .ok_or_else(|| anyhow::anyhow!("re-parse of dest failed after refile"))?;

    let inserted_info = resolve_section_ref(
        &dest_bytes,
        &dest_tree_new,
        &SectionRef::Id { file: None, id: final_custom_id.clone() },
    )?;

    let mut validation = validate_tree(&dest_bytes, &dest_tree_new)?;

    if !same_file {
        let mut src_parser = crate::parser::make_parser()?;
        let src_tree_new = src_parser
            .parse(&src_bytes, None)
            .ok_or_else(|| anyhow::anyhow!("re-parse of src failed after refile"))?;
        let src_report = validate_tree(&src_bytes, &src_tree_new)?;
        validation.errors.extend(src_report.errors);
        validation.warnings.extend(src_report.warnings);
    }

    Ok(RefileOutput {
        src_file: src_file.to_string(),
        dest_file: dest_file.to_string(),
        src_bytes,
        dest_bytes,
        final_custom_id,
        custom_id_changed,
        dest_start_line: inserted_info.start_line,
        src_title: src_info.title.clone(),
        validation,
    })
}

// ── org link parsing ──────────────────────────────────────────────────────────

/// Move the section identified by `src` to the position described by `dest`.
///
/// Reads and parses both files, resolves section references, runs the refile
/// algorithm, validates the result, and returns the modified bytes together
/// with the resolved file paths. The caller is responsible for writing
/// `src_bytes` to `src_file` and `dest_bytes` to `dest_file`.
///
/// The source section must already have a `:CUSTOM_ID:` — call
/// `ensure_custom_id` first if it does not.
pub fn refile_subtree(src: &SectionRef, dest: &Dest) -> Result<RefileOutput> {
    let src_file = src.file()
        .ok_or_else(|| anyhow::anyhow!("src.file is required"))?;

    let src_source = std::fs::read(src_file)
        .map_err(|e| anyhow::anyhow!("cannot read {src_file}: {e}"))?;
    let mut p = crate::parser::make_parser()?;
    let src_tree = p.parse(&src_source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {src_file}"))?;
    let src_info = resolve_section_ref(&src_source, &src_tree, src)?;

    let dest_file_path: &str = match dest {
        Dest::Before     { section }
        | Dest::After      { section }
        | Dest::FirstChild { section }
        | Dest::LastChild  { section } => section.file().unwrap_or(src_file),
        Dest::DocTop    { file }
        | Dest::DocBottom { file } => file.as_deref().unwrap_or(src_file),
    };

    let same_file = Path::new(src_file)
        .canonicalize()
        .and_then(|a| Path::new(dest_file_path).canonicalize().map(|b| a == b))
        .unwrap_or(false);

    let (dest_source, dest_tree) = if same_file {
        let mut p2 = crate::parser::make_parser()?;
        let t = p2.parse(&src_source, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {src_file}"))?;
        (src_source.clone(), t)
    } else {
        let bytes = std::fs::read(dest_file_path)
            .map_err(|e| anyhow::anyhow!("cannot read {dest_file_path}: {e}"))?;
        let mut p2 = crate::parser::make_parser()?;
        let t = p2.parse(&bytes, None)
            .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {dest_file_path}"))?;
        (bytes, t)
    };

    let dest_section_ref: Option<&SectionRef> = match dest {
        Dest::Before     { section }
        | Dest::After      { section }
        | Dest::FirstChild { section }
        | Dest::LastChild  { section } => Some(section),
        Dest::DocTop { .. } | Dest::DocBottom { .. } => None,
    };
    let dest_info: Option<SectionInfo> = dest_section_ref
        .map(|r| resolve_section_ref(&dest_source, &dest_tree, r))
        .transpose()?;

    refile_bytes(
        &src_source, src_file, &src_info,
        &dest_source, dest_file_path, &dest_tree,
        dest_info.as_ref(), dest, same_file,
    )
}

pub enum OrgLink {
    /// Link targeting a specific section (`[[#id]]`, `[[file:f.org::#id]]`,
    /// `[[file:f.org::*H/S]]`). The `file` field of the inner `SectionRef` is
    /// `None` for same-file links and `Some(path)` (possibly relative) otherwise.
    Section(SectionRef),
    /// Bare file link with no section target (`[[file:path.org]]`).
    Document(String),
}

pub fn parse_org_link(raw: &str) -> Result<OrgLink> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .map(|s| s.splitn(2, "][").next().unwrap_or(s))
        .unwrap_or(trimmed);

    if let Some(id) = inner.strip_prefix('#') {
        return Ok(OrgLink::Section(SectionRef::Id { file: None, id: id.to_string() }));
    }

    if let Some(rest) = inner.strip_prefix("file:") {
        return if let Some((path, target)) = rest.split_once("::") {
            let file = Some(path.to_string());
            if let Some(id) = target.strip_prefix('#') {
                Ok(OrgLink::Section(SectionRef::Id { file, id: id.to_string() }))
            } else if let Some(heading) = target.strip_prefix('*') {
                let path = heading.split('/').map(|s| s.trim().to_string()).collect();
                Ok(OrgLink::Section(SectionRef::Path { file, path }))
            } else {
                bail!("unsupported link target syntax: {target:?}")
            }
        } else {
            Ok(OrgLink::Document(rest.to_string()))
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
        let (new_bytes, new_text) = patch_subtree(
            src, &tree,
            &SectionRef::Id { file: None, id: "alpha".to_string() },
            "Content of alpha", "Updated content",
        ).unwrap();
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
        let err = patch_subtree(src, &tree, &SectionRef::Id { file: None, id: "nonexistent".to_string() }, "x", "y").unwrap_err();
        assert!(err.to_string().contains("CUSTOM_ID"));
    }

    #[test]
    fn patch_errors_when_search_not_found() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let err = patch_subtree(src, &tree, &SectionRef::Id { file: None, id: "alpha".to_string() }, "no such text", "y").unwrap_err();
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
        let info = section_for(src, &tree, &SectionRef::Line { file: None, line: 4 }).unwrap().unwrap();
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
        let info = section_for(src, &tree, &SectionRef::Id { file: None, id: "beta-sub".to_string() }).unwrap().unwrap();
        assert_eq!(info.title, "Beta Sub");
        assert_eq!(info.breadcrumbs, vec!["Beta"]);
    }

    #[test]
    fn section_for_by_heading_path() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        let path = vec!["Beta".to_string(), "Beta Sub".to_string()];
        let info = section_for(src, &tree, &SectionRef::Path { file: None, path }).unwrap().unwrap();
        assert_eq!(info.title, "Beta Sub");
    }

    #[test]
    fn section_for_picks_innermost_by_line() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 7 is inside Beta but NOT Beta Sub (which starts at row 9).
        let info = section_for(src, &tree, &SectionRef::Line { file: None, line: 7 }).unwrap().unwrap();
        assert_eq!(info.title, "Beta");
        // Row 13 is inside Beta Sub; line alone picks the innermost enclosing section.
        let info2 = section_for(src, &tree, &SectionRef::Line { file: None, line: 13 }).unwrap().unwrap();
        assert_eq!(info2.title, "Beta Sub");
        assert_eq!(info2.breadcrumbs, vec!["Beta"]);
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
        let err = section_for(src, &tree, &SectionRef::Path { file: None, path }).unwrap_err();
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
        // Row 1 is inside the first "Results" section; row 4 inside the second.
        let info = section_for(src, &tree, &SectionRef::Line { file: None, line: 1 }).unwrap().unwrap();
        assert_eq!(info.start_line, 0);
        let info2 = section_for(src, &tree, &SectionRef::Line { file: None, line: 4 }).unwrap().unwrap();
        assert_eq!(info2.start_line, 3);
    }

    #[test]
    fn ensure_custom_id_detects_existing() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 0 is the Alpha headline which already has CUSTOM_ID "alpha".
        let res = ensure_custom_id(src, &tree, &SectionRef::Line { file: None, line: 0 }, "anything").unwrap();
        assert!(res.already_existed);
        assert_eq!(res.custom_id, "alpha");
    }

    #[test]
    fn ensure_custom_id_inserts_new() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 7 is content inside Beta, which has no CUSTOM_ID.
        let res = ensure_custom_id(src, &tree, &SectionRef::Line { file: None, line: 7 }, "beta").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "beta");
        assert!(res.subtree.contains(":CUSTOM_ID: beta"));
    }

    #[test]
    fn ensure_custom_id_disambiguates_collision() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // "alpha" is already taken; Beta should get "alpha-2".
        let res = ensure_custom_id(src, &tree, &SectionRef::Line { file: None, line: 6 }, "alpha").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "alpha-2");
    }

    // ── validate ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_clean_file_has_no_errors() {
        let src = ORG.as_bytes();
        let report = validate(src).unwrap();
        assert!(report.errors.is_empty());
    }

    #[test]
    fn validate_reports_duplicate_custom_ids() {
        let src = b"\
* A
:PROPERTIES:
:CUSTOM_ID: same
:END:

* B
:PROPERTIES:
:CUSTOM_ID: same
:END:
";
        let report = validate(src).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.warnings.is_empty());
        assert!(report.errors[0].message.contains("same"));
    }

    // ── refile_subtree ────────────────────────────────────────────────────────

    fn resolve(src: &[u8], tree: &tree_sitter::Tree, cid: &str) -> SectionInfo {
        section_for(src, tree, &SectionRef::Id { file: None, id: cid.to_string() }).unwrap().unwrap()
    }

    #[test]
    fn refile_after_cross_file() {
        let src = ORG.as_bytes();
        let dest = b"\
* Existing
:PROPERTIES:
:CUSTOM_ID: existing
:END:
Content.
";
        let st = parse(src);
        let dt = parse(dest);
        let src_info  = resolve(src, &st, "alpha");
        let dest_info = resolve(dest, &dt, "existing");
        let out = refile_bytes(src, "", &src_info, dest, "", &dt, Some(&dest_info), &Dest::After { section: SectionRef::Id { file: None, id: String::new() } }, false).unwrap();

        assert!(out.validation.errors.is_empty());
        assert_eq!(out.final_custom_id, "alpha");
        assert!(!out.custom_id_changed);

        let dest_str = std::str::from_utf8(&out.dest_bytes).unwrap();
        assert!(dest_str.contains("* Alpha"), "headline present in dest");
        assert!(dest_str.contains(":CUSTOM_ID: alpha"));

        let src_str = std::str::from_utf8(&out.src_bytes).unwrap();
        assert!(!src_str.contains("Content of alpha"), "section removed from src");
        assert!(src_str.contains("Beta"), "other sections intact");
    }

    #[test]
    fn refile_disambiguates_custom_id_collision() {
        let src  = ORG.as_bytes();
        let dest = b"\
* Alpha Copy
:PROPERTIES:
:CUSTOM_ID: alpha
:END:
";
        let st = parse(src);
        let dt = parse(dest);
        let src_info = resolve(src, &st, "alpha");
        let out = refile_bytes(src, "", &src_info, dest, "", &dt, None, &Dest::DocBottom { file: None }, false).unwrap();

        assert!(out.custom_id_changed);
        assert_eq!(out.final_custom_id, "alpha-2");
        let dest_str = std::str::from_utf8(&out.dest_bytes).unwrap();
        assert!(dest_str.contains(":CUSTOM_ID: alpha-2"));
    }

    #[test]
    fn refile_adjusts_depth_child_first() {
        let src  = ORG.as_bytes();
        let dest = b"\
* Container
:PROPERTIES:
:CUSTOM_ID: container
:END:
";
        let st = parse(src);
        let dt = parse(dest);
        let src_info  = resolve(src, &st, "alpha");
        let dest_info = resolve(dest, &dt, "container");
        let out = refile_bytes(src, "", &src_info, dest, "", &dt, Some(&dest_info), &Dest::FirstChild { section: SectionRef::Id { file: None, id: String::new() } }, false).unwrap();

        assert!(out.validation.errors.is_empty());
        let dest_str = std::str::from_utf8(&out.dest_bytes).unwrap();
        assert!(dest_str.contains("** Alpha"), "depth increased to 2");
    }

    #[test]
    fn refile_doc_bottom() {
        let src  = ORG.as_bytes();
        let dest = b"* Only\n:PROPERTIES:\n:CUSTOM_ID: only\n:END:\n";
        let st = parse(src);
        let dt = parse(dest);
        let src_info = resolve(src, &st, "alpha");
        let out = refile_bytes(src, "", &src_info, dest, "", &dt, None, &Dest::DocBottom { file: None }, false).unwrap();

        assert!(out.validation.errors.is_empty());
        let dest_str = std::str::from_utf8(&out.dest_bytes).unwrap();
        let pos_only  = dest_str.find("* Only").unwrap();
        let pos_alpha = dest_str.find("* Alpha").unwrap();
        assert!(pos_alpha > pos_only);
    }

    #[test]
    fn refile_same_file() {
        let src  = ORG.as_bytes();
        let tree = parse(src);
        let src_info  = resolve(src, &tree, "alpha");
        let dest_info = resolve(src, &tree, "beta-sub");
        let out = refile_bytes(src, "", &src_info, src, "", &tree, Some(&dest_info), &Dest::After { section: SectionRef::Id { file: None, id: String::new() } }, true).unwrap();

        assert!(out.validation.errors.is_empty());
        assert_eq!(out.src_bytes, out.dest_bytes);
        // CUSTOM_ID must be preserved — same-file refile is not a collision.
        assert!(!out.custom_id_changed, "same-file refile must not rename CUSTOM_ID");
        let text = std::str::from_utf8(&out.dest_bytes).unwrap();
        assert!(text.contains("** Alpha"), "depth matches beta-sub sibling (depth 2)");
        assert!(text.contains("** Beta Sub"));
    }

    #[test]
    fn refile_same_file_preserves_custom_id() {
        // Regression: the collision check was running against the pre-removal file,
        // finding the src section's own ID and spuriously renaming it to "<id>-2".
        let src  = ORG.as_bytes();
        let tree = parse(src);
        let src_info  = resolve(src, &tree, "alpha");
        let dest_info = resolve(src, &tree, "beta-sub");
        let out = refile_bytes(src, "", &src_info, src, "", &tree, Some(&dest_info),
            &Dest::Before { section: SectionRef::Id { file: None, id: String::new() } }, true).unwrap();
        assert_eq!(out.final_custom_id, "alpha");
        assert!(!out.custom_id_changed);
    }

    #[test]
    fn ensure_custom_id_by_headline_row() {
        let src = ORG.as_bytes();
        let tree = parse(src);
        // Row 6 is the Beta headline itself — still resolves to the Beta section.
        let res = ensure_custom_id(src, &tree, &SectionRef::Line { file: None, line: 6 }, "beta-new").unwrap();
        assert!(!res.already_existed);
        assert_eq!(res.custom_id, "beta-new");
    }
}
