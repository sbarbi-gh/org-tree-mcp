//! Read-only backlink discovery and broken-link auditing across a directory
//! of org files. Built on the same link-matching tree-sitter query documented
//! in README.org, plus a variant for bare same-file `[[#id]]` links.
//!
//! Both [`BacklinkHit`] and [`LinkCheckError`] carry the matched link node's
//! byte `range`, not just printable strings, so a future "rewrite backlinks
//! pointing at X" pass can splice new text directly into these ranges.

use anyhow::{anyhow, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::ops::{collect_org_files, parse_and_run};
use crate::query::{breadcrumbs, byte_range, query_nodes};
use crate::{make_parser, parse_org_link, resolve_section_ref, ByteRange, OrgLink, SectionRef};

/// Matches both `[[file:...]]` links and bare same-file `[[#id]]` links —
/// the grammar tokenizes each as a single atomic `expr`.
pub const LINK_QUERY: &str = r#"
(_ (expr) @link (#match? @link "^\\[\\[file:"))
(_ (expr) @link (#match? @link "^\\[\\[#"))
"#;

fn resolve_relative_to(base_file: &Path, raw_path: &str) -> PathBuf {
    let p = Path::new(raw_path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    base_file.parent().unwrap_or_else(|| Path::new(".")).join(p)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ── backlinks ─────────────────────────────────────────────────────────────────

/// A file (optionally narrowed to one section) to find inbound links for.
#[derive(Debug, Clone)]
pub struct BacklinkTarget {
    pub file: PathBuf,
    /// Narrow to links whose `::#id` fragment matches (mutually exclusive
    /// with `heading_path` — callers should set at most one).
    pub id: Option<String>,
    /// Narrow to links whose `::*H/S` fragment matches, case-insensitively,
    /// element by element.
    pub heading_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacklinkHit {
    pub source_file: PathBuf,
    pub line: usize,
    pub breadcrumbs: Vec<String>,
    pub link_text: String,
    pub range: ByteRange,
}

fn heading_paths_match(link_path: &[String], target_path: &[String]) -> bool {
    link_path.len() == target_path.len()
        && link_path.iter().zip(target_path).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn link_targets(link: &OrgLink, target: &BacklinkTarget, current_file: &Path) -> bool {
    match link {
        OrgLink::Document(doc_path) => {
            target.id.is_none()
                && target.heading_path.is_none()
                && same_path(&resolve_relative_to(current_file, doc_path), &target.file)
        }
        OrgLink::Section(r) => {
            let resolved = match r.file() {
                Some(f) => resolve_relative_to(current_file, f),
                None => current_file.to_path_buf(),
            };
            if !same_path(&resolved, &target.file) {
                return false;
            }
            match (&target.id, &target.heading_path, r) {
                (None, None, _) => true,
                (Some(id), _, SectionRef::Id { id: rid, .. }) => rid == id,
                (_, Some(hp), SectionRef::Path { path: rp, .. }) => heading_paths_match(rp, hp),
                _ => false,
            }
        }
    }
}

/// Find every link in every `.org` file under `root` that resolves to
/// `target.file` (optionally narrowed to `target.id` or `target.heading_path`).
pub fn find_backlinks(root: &Path, target: &BacklinkTarget) -> Result<Vec<BacklinkHit>> {
    let target = BacklinkTarget {
        file: target
            .file
            .canonicalize()
            .map_err(|e| anyhow!("cannot find target file {}: {e}", target.file.display()))?,
        id: target.id.clone(),
        heading_path: target.heading_path.clone(),
    };

    let mut hits = Vec::new();
    for path in collect_org_files(root)? {
        let path_str = path.to_string_lossy().to_string();
        let file_hits = parse_and_run(&path_str, |source, tree| {
            let nodes = query_nodes(source, tree.root_node(), LINK_QUERY, |n, _| Some(n))?;
            let mut file_hits = Vec::new();
            for node in nodes {
                let text = node.utf8_text(source).unwrap_or("").to_string();
                let Ok(link) = parse_org_link(&text) else { continue };
                if link_targets(&link, &target, &path) {
                    file_hits.push(BacklinkHit {
                        source_file: path.clone(),
                        line: node.start_position().row,
                        breadcrumbs: breadcrumbs(node, source),
                        link_text: text,
                        range: byte_range(&node),
                    });
                }
            }
            Ok(file_hits)
        });
        match file_hits {
            Ok(mut v) => hits.append(&mut v),
            Err(e) => eprintln!("warn: skipping {path_str}: {e}"),
        }
    }
    Ok(hits)
}

// ── check-links ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LinkCheckError {
    pub source_file: PathBuf,
    pub line: usize,
    pub link_text: String,
    pub error: String,
    pub range: ByteRange,
}

fn check_one_link(current_file: &Path, source: &[u8], tree: &tree_sitter::Tree, text: &str) -> Result<()> {
    match parse_org_link(text)? {
        OrgLink::Document(doc_path) => {
            let resolved = resolve_relative_to(current_file, &doc_path);
            std::fs::metadata(&resolved)
                .map(|_| ())
                .map_err(|e| anyhow!("cannot find {}: {e}", resolved.display()))
        }
        OrgLink::Section(r) => match r.file() {
            None => resolve_section_ref(source, tree, &r).map(|_| ()),
            Some(f) => {
                let resolved = resolve_relative_to(current_file, f);
                let bytes = std::fs::read(&resolved)
                    .map_err(|e| anyhow!("cannot read {}: {e}", resolved.display()))?;
                let mut parser = make_parser()?;
                let target_tree = parser.parse(&bytes, None).ok_or_else(|| {
                    anyhow!("tree-sitter failed to parse {}", resolved.display())
                })?;
                resolve_section_ref(&bytes, &target_tree, &r).map(|_| ())
            }
        },
    }
}

/// Scan every `.org` file under `root` for links that fail to resolve
/// (dangling file paths, missing CUSTOM_IDs, missing heading paths).
pub fn check_links(root: &Path) -> Result<Vec<LinkCheckError>> {
    let mut errors = Vec::new();
    for path in collect_org_files(root)? {
        let path_str = path.to_string_lossy().to_string();
        let file_errors = parse_and_run(&path_str, |source, tree| {
            let nodes = query_nodes(source, tree.root_node(), LINK_QUERY, |n, _| Some(n))?;
            let mut file_errors = Vec::new();
            for node in nodes {
                let text = node.utf8_text(source).unwrap_or("").to_string();
                if let Err(e) = check_one_link(&path, source, tree, &text) {
                    file_errors.push(LinkCheckError {
                        source_file: path.clone(),
                        line: node.start_position().row,
                        link_text: text,
                        error: e.to_string(),
                        range: byte_range(&node),
                    });
                }
            }
            Ok(file_errors)
        });
        match file_errors {
            Ok(mut v) => errors.append(&mut v),
            Err(e) => eprintln!("warn: skipping {path_str}: {e}"),
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "org-parser-links-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixture_tree(dir: &Path) {
        std::fs::write(
            dir.join("a.org"),
            "\
* Alpha
:PROPERTIES:
:CUSTOM_ID: alpha
:END:
See [[file:b.org::#beta]] for details.
",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.org"),
            "\
* Beta
:PROPERTIES:
:CUSTOM_ID: beta
:END:
Back to [[file:a.org::#alpha]] and also [[file:a.org::*Alpha]]
",
        )
        .unwrap();
        std::fs::write(
            dir.join("c.org"),
            "* Unrelated\nNothing here.\n",
        )
        .unwrap();
    }

    #[test]
    fn find_backlinks_unnarrowed_finds_all_links_to_file() {
        let dir = tempdir();
        write_fixture_tree(&dir);
        let target = BacklinkTarget { file: dir.join("a.org"), id: None, heading_path: None };
        let hits = find_backlinks(&dir, &target).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source_file.ends_with("b.org")));
        assert_eq!(hits[0].breadcrumbs, vec!["Beta".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_backlinks_narrowed_by_id() {
        let dir = tempdir();
        write_fixture_tree(&dir);
        let target =
            BacklinkTarget { file: dir.join("a.org"), id: Some("alpha".to_string()), heading_path: None };
        let hits = find_backlinks(&dir, &target).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].link_text.contains("#alpha"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_backlinks_narrowed_by_heading() {
        let dir = tempdir();
        write_fixture_tree(&dir);
        let target = BacklinkTarget {
            file: dir.join("a.org"),
            id: None,
            heading_path: Some(vec!["Alpha".to_string()]),
        };
        let hits = find_backlinks(&dir, &target).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].link_text.contains("*Alpha"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_links_clean_tree_has_no_errors() {
        let dir = tempdir();
        write_fixture_tree(&dir);
        let errors = check_links(&dir).unwrap();
        assert!(errors.is_empty(), "expected no errors, got {errors:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_links_reports_dangling_and_broken_links() {
        let dir = tempdir();
        write_fixture_tree(&dir);
        std::fs::write(
            dir.join("d.org"),
            "\
* Dangling
Link to [[file:nope.org]] which doesn't exist.
Also broken same-file [[#not-real]]
",
        )
        .unwrap();
        let errors = check_links(&dir).unwrap();
        assert_eq!(errors.len(), 2, "expected 2 errors, got {errors:?}");
        assert!(errors.iter().any(|e| e.link_text.contains("nope.org")));
        assert!(errors.iter().any(|e| e.link_text.contains("#not-real")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
