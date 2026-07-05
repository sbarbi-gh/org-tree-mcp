//! Shared file-I/O/orchestration layer used by both the `org` CLI and the
//! `org-mcp` server: read → parse → call a core `query` function → validate →
//! write. Keeping this in one place means both adapters get the same
//! behavior (including the Emacs lockfile guardrail) for free.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::{
    check_not_locked, ensure_custom_id as core_ensure_custom_id,
    insert_subtree as core_insert_subtree, make_parser, parse_org_link,
    patch_subtree as core_patch_subtree, refile_subtree as core_refile_subtree,
    resolve_section_ref, run_query, validate, Dest, EnsureCustomIdResult, FilePatch, InsertOutput,
    OrgLink, QueryMatch, RefileOutput, SectionInfo, SectionRef, ValidationReport,
};

// ── read/parse ────────────────────────────────────────────────────────────────

pub fn parse_and_run<F, T>(file: &str, f: F) -> Result<T>
where
    F: FnOnce(&[u8], &tree_sitter::Tree) -> Result<T>,
{
    let source = std::fs::read(file).map_err(|e| anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {file}"))?;
    f(&source, &tree)
}

pub fn compile_patterns(raw: &[String]) -> Result<Vec<Regex>> {
    raw.iter()
        .map(|p| Regex::new(p).map_err(|e| anyhow!("invalid pattern {p:?}: {e}")))
        .collect()
}

// ── directory walk ────────────────────────────────────────────────────────────

pub fn collect_org_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_org_files_rec(dir, &mut files)?;
    Ok(files)
}

fn collect_org_files_rec(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| anyhow!("cannot read dir {}: {e}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_org_files_rec(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("org") {
            out.push(path);
        }
    }
    Ok(())
}

// ── query ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FileMatch {
    pub file: String,
    #[serde(flatten)]
    pub m: QueryMatch,
}

/// Run a tree-sitter query against `path` — a single file, or every `*.org`
/// file under it recursively if it's a directory.
pub fn query_path(path: &str, query_src: &str, patterns: &[Regex]) -> Result<Vec<FileMatch>> {
    if Path::new(path).is_dir() {
        search_directory(path, query_src, patterns)
    } else {
        let file = path.to_string();
        parse_and_run(path, move |src, tree| {
            let matches = run_query(src, tree, query_src, patterns)?;
            Ok(matches
                .into_iter()
                .map(|m| FileMatch { file: file.clone(), m })
                .collect())
        })
    }
}

fn search_directory(dir: &str, query_src: &str, patterns: &[Regex]) -> Result<Vec<FileMatch>> {
    let mut all = Vec::new();
    for path in collect_org_files(Path::new(dir))? {
        let path_str = path.to_string_lossy().to_string();
        match parse_and_run(&path_str, |src, tree| run_query(src, tree, query_src, patterns)) {
            Ok(matches) => all.extend(matches.into_iter().map(|m| FileMatch {
                file: path_str.clone(),
                m,
            })),
            Err(e) => eprintln!("warn: skipping {path_str}: {e}"),
        }
    }
    Ok(all)
}

// ── open_link ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LinkedSection {
    pub file: String,
    #[serde(flatten)]
    pub info: SectionInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkedFile {
    pub file: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum LinkTarget {
    Section(LinkedSection),
    Document(LinkedFile),
}

/// Follow an Org-mode link string, resolving relative paths against
/// `base_file` (a file or directory).
pub fn follow_org_link(link: &str, base_file: Option<&str>) -> Result<LinkTarget> {
    let resolve = |f: &str| -> Result<String> {
        if Path::new(f).is_absolute() {
            return Ok(f.to_string());
        }
        let base = base_file
            .ok_or_else(|| anyhow!("base_file required to resolve relative path {f:?}"))?;
        let base_path = Path::new(base);
        let base_dir = if base_path.is_dir() {
            base_path
        } else {
            base_path
                .parent()
                .ok_or_else(|| anyhow!("cannot determine parent dir of {base:?}"))?
        };
        Ok(base_dir.join(f).to_string_lossy().into_owned())
    };
    let require_base = || {
        base_file
            .ok_or_else(|| anyhow!("base_file required for same-file link"))
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
                Ok(LinkTarget::Section(LinkedSection { file: file_for_result, info }))
            })
        }
        OrgLink::Document(path) => {
            let file = resolve(&path)?;
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow!("cannot read {file}: {e}"))?;
            Ok(LinkTarget::Document(LinkedFile { file, content }))
        }
    }
}

// ── patch_subtree ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PatchReport {
    pub subtree: String,
    pub patch: FilePatch,
}

fn validation_error(report: &ValidationReport) -> anyhow::Error {
    anyhow!(
        "write aborted — validation errors: {}",
        report.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
    )
}

pub fn run_patch(
    file: &str,
    r: &SectionRef,
    search: &str,
    replace: &str,
    force: bool,
) -> Result<PatchReport> {
    let source = std::fs::read(file).map_err(|e| anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {file}"))?;
    let (modified_bytes, new_section, patch) =
        core_patch_subtree(file, &source, &tree, r, search, replace)?;
    let report = validate(&modified_bytes)?;
    if report.has_errors() {
        return Err(validation_error(&report));
    }
    check_not_locked(Path::new(file), force)?;
    std::fs::write(file, &modified_bytes).map_err(|e| anyhow!("cannot write {file}: {e}"))?;
    Ok(PatchReport { subtree: new_section, patch })
}

// ── ensure_custom_id ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EnsureCustomIdReport {
    pub custom_id: String,
    pub already_existed: bool,
    pub patch: Option<FilePatch>,
}

pub fn run_ensure_custom_id(
    file: &str,
    line: usize,
    proposed_id: &str,
    force: bool,
) -> Result<EnsureCustomIdReport> {
    let source = std::fs::read(file).map_err(|e| anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {file}"))?;
    let r = SectionRef::Line { file: None, line };
    let EnsureCustomIdResult { custom_id, file_content, patch, already_existed } =
        core_ensure_custom_id(&source, &tree, &r, proposed_id)?;
    if !already_existed {
        let report = validate(&file_content)?;
        if report.has_errors() {
            return Err(validation_error(&report));
        }
        check_not_locked(Path::new(file), force)?;
        std::fs::write(file, &file_content).map_err(|e| anyhow!("cannot write {file}: {e}"))?;
    }
    Ok(EnsureCustomIdReport { custom_id, already_existed, patch })
}

// ── refile_subtree ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct RefileSrcSummary {
    pub file: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefileDestSummary {
    pub file: String,
    pub custom_id: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefileReport {
    pub src: RefileSrcSummary,
    pub dest: RefileDestSummary,
    pub custom_id_changed: bool,
    pub validation: ValidationReport,
}

pub fn run_refile(src_ref: &SectionRef, dest: &Dest, force: bool) -> Result<RefileReport> {
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
    } = core_refile_subtree(src_ref, dest)?;

    if validation.has_errors() {
        return Err(validation_error(&validation));
    }

    let same_file = src_file == dest_file;
    if same_file {
        check_not_locked(Path::new(&dest_file), force)?;
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow!("cannot write {dest_file}: {e}"))?;
    } else {
        check_not_locked(Path::new(&src_file), force)?;
        check_not_locked(Path::new(&dest_file), force)?;
        std::fs::write(&src_file, &src_bytes)
            .map_err(|e| anyhow!("cannot write {src_file}: {e}"))?;
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow!("cannot write {dest_file}: {e}"))?;
    }

    Ok(RefileReport {
        src: RefileSrcSummary { file: src_file, title: src_title },
        dest: RefileDestSummary { file: dest_file, custom_id: final_custom_id, line: dest_start_line },
        custom_id_changed,
        validation,
    })
}

// ── insert_subtree ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct InsertDestSummary {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct InsertReport {
    pub dest: InsertDestSummary,
    pub validation: ValidationReport,
}

pub fn run_insert(content: &str, dest: &Dest, force: bool) -> Result<InsertReport> {
    let InsertOutput { dest_file, dest_bytes, dest_start_line, validation } =
        core_insert_subtree(content, dest)?;
    if validation.has_errors() {
        return Err(validation_error(&validation));
    }
    check_not_locked(Path::new(&dest_file), force)?;
    std::fs::write(&dest_file, &dest_bytes)
        .map_err(|e| anyhow!("cannot write {dest_file}: {e}"))?;
    Ok(InsertReport {
        dest: InsertDestSummary { file: dest_file, line: dest_start_line },
        validation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "org-parser-ops-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const ORG: &str = "\
* Alpha
:PROPERTIES:
:CUSTOM_ID: alpha
:END:
Content of alpha.
";

    #[test]
    #[cfg(unix)]
    fn run_patch_blocked_by_lock_and_force_overrides() {
        let dir = tempdir();
        let file = dir.join("foo.org");
        std::fs::write(&file, ORG).unwrap();
        let lock = dir.join(".#foo.org");
        symlink("alice@host.1:1", &lock).unwrap();

        let file_str = file.to_string_lossy().to_string();
        let r = SectionRef::Id { file: None, id: "alpha".to_string() };

        let err = run_patch(&file_str, &r, "Content of alpha", "Updated", false).unwrap_err();
        assert!(err.to_string().contains("locked by Emacs"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), ORG);

        let report = run_patch(&file_str, &r, "Content of alpha", "Updated", true).unwrap();
        assert!(report.subtree.contains("Updated"));
        assert!(std::fs::read_to_string(&file).unwrap().contains("Updated"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn run_ensure_custom_id_blocked_by_lock() {
        let dir = tempdir();
        let no_id_org = "* Beta\nSome content.\n";
        let file = dir.join("bar.org");
        std::fs::write(&file, no_id_org).unwrap();
        let lock = dir.join(".#bar.org");
        symlink("alice@host.1:1", &lock).unwrap();

        let file_str = file.to_string_lossy().to_string();
        let err = run_ensure_custom_id(&file_str, 0, "beta", false).unwrap_err();
        assert!(err.to_string().contains("locked by Emacs"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), no_id_org);

        let report = run_ensure_custom_id(&file_str, 0, "beta", true).unwrap();
        assert_eq!(report.custom_id, "beta");
        assert!(!report.already_existed);
        assert!(std::fs::read_to_string(&file).unwrap().contains("CUSTOM_ID: beta"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_path_single_file() {
        let dir = tempdir();
        let file = dir.join("foo.org");
        std::fs::write(&file, ORG).unwrap();
        let file_str = file.to_string_lossy().to_string();
        let matches = query_path(&file_str, "(section) @section", &[]).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, file_str);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_path_directory() {
        let dir = tempdir();
        std::fs::write(dir.join("a.org"), ORG).unwrap();
        std::fs::write(dir.join("b.org"), ORG).unwrap();
        let matches = query_path(dir.to_string_lossy().as_ref(), "(section) @section", &[]).unwrap();
        assert_eq!(matches.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
