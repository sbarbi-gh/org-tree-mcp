use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use regex::Regex;
use std::path::Path;

use org_parser::{
    ensure_custom_id as org_ensure_custom_id, make_parser, outline, parse_org_link,
    patch_subtree as org_patch_subtree, refile_subtree as org_refile_subtree,
    resolve_section_ref, run_query, validate as org_validate,
    Dest, EnsureCustomIdResult, OrgLink, RefileOutput, SectionRef,
};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "org", about = "Structural navigation and editing of Org mode files")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List all headlines in an org file (JSON array).
    Outline {
        file: String,
    },

    /// Run a tree-sitter S-expression query against a file or directory.
    Query {
        /// Org file or directory to search (recursively for directories).
        path: String,
        /// Tree-sitter S-expression query string.
        query: String,
        /// Regex patterns to AND-filter results by matched text (repeatable).
        #[arg(short, long = "pattern")]
        patterns: Vec<String>,
    },

    /// Print full metadata and org text for a section.
    Subtree {
        file: String,
        /// Identify by :CUSTOM_ID: value.
        #[arg(long)]
        id: Option<String>,
        /// Identify by 0-indexed line number (from outline/query row).
        #[arg(long)]
        line: Option<usize>,
        /// Heading path: one element per level, case-insensitive regex (repeatable).
        #[arg(long = "heading")]
        heading: Vec<String>,
    },

    /// Follow an Org-mode link and print the target section or file content.
    OpenLink {
        /// The link string (with or without outer [[ ]]).
        link: String,
        /// File used to resolve same-file and relative links.
        #[arg(long)]
        base: Option<String>,
    },

    /// Print documented tree-sitter query examples for the org grammar.
    QueryExamples,

    /// Search and replace text within the subtree identified by a CUSTOM_ID.
    PatchSubtree {
        file: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        search: String,
        #[arg(long)]
        replace: String,
    },

    /// Ensure the section at the given line has a :CUSTOM_ID:, inserting one if absent.
    EnsureCustomId {
        file: String,
        /// 0-indexed line number of the target section.
        #[arg(long)]
        line: usize,
        /// Proposed CUSTOM_ID value (auto-disambiguated with -2/-3/… suffix if taken).
        #[arg(long)]
        id: String,
    },

    /// Move a section (by CUSTOM_ID) within or between org files.
    Refile {
        /// Source file.
        src_file: String,
        /// CUSTOM_ID of the section to move.
        src_id: String,
        /// Placement: before|after|first-child|last-child|doc-top|doc-bottom.
        #[arg(long)]
        placement: String,
        /// Destination file (defaults to src-file for doc-top/doc-bottom;
        /// required for cross-file section-relative placements).
        #[arg(long)]
        dest_file: Option<String>,
        /// CUSTOM_ID of the destination anchor section
        /// (required for before/after/first-child/last-child).
        #[arg(long)]
        dest_id: Option<String>,
        /// Line number of the destination anchor section (alternative to --dest-id).
        #[arg(long)]
        dest_line: Option<usize>,
    },
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli.cmd) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Outline { file } => {
            let out = parse_and_run(&file, |src, tree| {
                let entries = outline(src, tree)?;
                Ok(serde_json::to_string_pretty(&entries)?)
            })?;
            println!("{out}");
        }

        Cmd::Query { path, query, patterns } => {
            let compiled = compile_patterns(&patterns)?;
            let out = if Path::new(&path).is_dir() {
                search_directory(&path, &query, &compiled)?
            } else {
                parse_and_run(&path, |src, tree| {
                    let matches = run_query(src, tree, &query, &compiled)?;
                    let with_file: Vec<_> = matches
                        .into_iter()
                        .map(|m| {
                            let mut v = serde_json::to_value(m).unwrap();
                            v.as_object_mut().unwrap().insert("file".into(), path.clone().into());
                            v
                        })
                        .collect();
                    Ok(serde_json::to_string_pretty(&with_file)?)
                })?
            };
            println!("{out}");
        }

        Cmd::Subtree { file, id, line, heading } => {
            let r = if let Some(id) = id {
                SectionRef::Id { file: None, id }
            } else if let Some(n) = line {
                SectionRef::Line { file: None, line: n }
            } else if !heading.is_empty() {
                SectionRef::Path { file: None, path: heading }
            } else {
                bail!("provide at least one of --id, --line, or --heading");
            };
            let out = parse_and_run(&file, |src, tree| {
                let info = resolve_section_ref(src, tree, &r)?;
                Ok(serde_json::to_string_pretty(&info)?)
            })?;
            println!("{out}");
        }

        Cmd::OpenLink { link, base } => {
            let out = follow_org_link(&link, base.as_deref())?;
            println!("{out}");
        }

        Cmd::QueryExamples => {
            print!("{}", org_parser::QUERY_EXAMPLES);
        }

        Cmd::PatchSubtree { file, id, search, replace } => {
            let out = run_patch(&file, &id, &search, &replace)?;
            println!("{out}");
        }

        Cmd::EnsureCustomId { file, line, id } => {
            let out = run_ensure_custom_id(&file, line, &id)?;
            println!("{out}");
        }

        Cmd::Refile { src_file, src_id, placement, dest_file, dest_id, dest_line } => {
            let src = SectionRef::Id { file: Some(src_file), id: src_id };
            let dest = build_dest(&placement, dest_file, dest_id, dest_line)?;
            let out = run_refile(&src, &dest)?;
            println!("{out}");
        }
    }
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_and_run<F, T>(file: &str, f: F) -> Result<T>
where
    F: FnOnce(&[u8], &tree_sitter::Tree) -> Result<T>,
{
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    f(&source, &tree)
}

fn compile_patterns(raw: &[String]) -> Result<Vec<Regex>> {
    raw.iter()
        .map(|p| Regex::new(p).map_err(|e| anyhow::anyhow!("invalid pattern {p:?}: {e}")))
        .collect()
}

fn search_directory(dir: &str, query_src: &str, patterns: &[Regex]) -> Result<String> {
    let mut all: Vec<serde_json::Value> = Vec::new();
    for path in collect_org_files(Path::new(dir))? {
        let path_str = path.to_string_lossy().to_string();
        match parse_and_run(&path_str, |src, tree| run_query(src, tree, query_src, patterns)) {
            Ok(matches) => {
                for m in matches {
                    let mut v = serde_json::to_value(m)?;
                    v.as_object_mut().unwrap().insert("file".into(), path_str.clone().into());
                    all.push(v);
                }
            }
            Err(e) => eprintln!("warn: skipping {path_str}: {e}"),
        }
    }
    Ok(serde_json::to_string_pretty(&all)?)
}

fn collect_org_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    collect_org_files_rec(dir, &mut files)?;
    Ok(files)
}

fn collect_org_files_rec(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read dir {}: {e}", dir.display()))?
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

fn follow_org_link(link: &str, base_file: Option<&str>) -> Result<String> {
    let resolve = |f: &str| -> Result<String> {
        if Path::new(f).is_absolute() {
            return Ok(f.to_string());
        }
        let base = base_file
            .ok_or_else(|| anyhow::anyhow!("--base required to resolve relative path {f:?}"))?;
        let base_path = Path::new(base);
        let base_dir = if base_path.is_dir() {
            base_path
        } else {
            base_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cannot determine parent dir of {base:?}"))?
        };
        Ok(base_dir.join(f).to_string_lossy().into_owned())
    };
    let require_base = || {
        base_file
            .ok_or_else(|| anyhow::anyhow!("--base required for same-file link"))
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
                let mut v = serde_json::to_value(info)?;
                v.as_object_mut().unwrap().insert("file".into(), file_for_result.into());
                Ok(serde_json::to_string_pretty(&v)?)
            })
        }
        OrgLink::Document(path) => {
            let file = resolve(&path)?;
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({ "file": file, "content": content }),
            )?)
        }
    }
}

fn run_patch(file: &str, custom_id: &str, search: &str, replace: &str) -> Result<String> {
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    let r = SectionRef::Id { file: None, id: custom_id.to_string() };
    let (modified_bytes, new_section) = org_patch_subtree(&source, &tree, &r, search, replace)?;
    let report = org_validate(&modified_bytes)?;
    if report.has_errors() {
        bail!(
            "write aborted — validation errors: {}",
            report.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
        );
    }
    std::fs::write(file, &modified_bytes)
        .map_err(|e| anyhow::anyhow!("cannot write {file}: {e}"))?;
    Ok(new_section)
}

fn run_ensure_custom_id(file: &str, line: usize, proposed_id: &str) -> Result<String> {
    let source = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
    let mut parser = make_parser()?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {file}"))?;
    let r = SectionRef::Line { file: None, line };
    let EnsureCustomIdResult { custom_id, subtree, file_content, already_existed } =
        org_ensure_custom_id(&source, &tree, &r, proposed_id)?;
    if !already_existed {
        let report = org_validate(&file_content)?;
        if report.has_errors() {
            bail!(
                "write aborted — validation errors: {}",
                report.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
            );
        }
        std::fs::write(file, &file_content)
            .map_err(|e| anyhow::anyhow!("cannot write {file}: {e}"))?;
    }
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "custom_id": custom_id,
        "already_existed": already_existed,
        "subtree": subtree,
    }))?)
}

fn build_dest(
    placement: &str,
    dest_file: Option<String>,
    dest_id: Option<String>,
    dest_line: Option<usize>,
) -> Result<Dest> {
    let anchor_ref = |label: &str| -> Result<SectionRef> {
        if let Some(id) = dest_id.clone() {
            Ok(SectionRef::Id { file: dest_file.clone(), id })
        } else if let Some(n) = dest_line {
            Ok(SectionRef::Line { file: dest_file.clone(), line: n })
        } else {
            bail!("--dest-id or --dest-line required for placement '{label}'")
        }
    };
    match placement {
        "before"      => Ok(Dest::Before     { section: anchor_ref("before")? }),
        "after"       => Ok(Dest::After      { section: anchor_ref("after")? }),
        "first-child" => Ok(Dest::FirstChild { section: anchor_ref("first-child")? }),
        "last-child"  => Ok(Dest::LastChild  { section: anchor_ref("last-child")? }),
        "doc-top"     => Ok(Dest::DocTop     { file: dest_file }),
        "doc-bottom"  => Ok(Dest::DocBottom  { file: dest_file }),
        other => bail!("unknown placement {other:?}; use before|after|first-child|last-child|doc-top|doc-bottom"),
    }
}

fn run_refile(src_ref: &SectionRef, dest: &Dest) -> Result<String> {
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
    } = org_refile_subtree(src_ref, dest)?;

    if validation.has_errors() {
        bail!(
            "write aborted — validation errors: {}",
            validation.errors.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ")
        );
    }

    if src_file == dest_file {
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {dest_file}: {e}"))?;
    } else {
        std::fs::write(&src_file, &src_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {src_file}: {e}"))?;
        std::fs::write(&dest_file, &dest_bytes)
            .map_err(|e| anyhow::anyhow!("cannot write {dest_file}: {e}"))?;
    }

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "src": { "file": src_file, "title": src_title },
        "dest": {
            "file": dest_file,
            "custom_id": final_custom_id,
            "line": dest_start_line,
        },
        "custom_id_changed": custom_id_changed,
        "validation": validation,
    }))?)
}
