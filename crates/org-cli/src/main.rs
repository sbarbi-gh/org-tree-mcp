use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};

use org_parser::links::{check_links, find_backlinks, BacklinkHit, BacklinkTarget, LinkCheckError};
use org_parser::ops::{
    self, EnsureCustomIdReport, FileMatch, InsertReport, LinkTarget, PatchReport, RefileReport,
};
use org_parser::{Dest, HeadlineEntry, SectionInfo, SectionRef, ValidationReport};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "org", about = "Structural navigation and editing of Org mode files")]
struct Cli {
    /// Output raw JSON instead of human-readable plain text.
    #[arg(short = 'j', long = "json", global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List all headlines in an org file.
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

    /// Search and replace text within a subtree (identify by --id, --line, or --heading).
    PatchSubtree {
        file: String,
        /// :CUSTOM_ID: value (preferred).
        #[arg(long)]
        id: Option<String>,
        /// 0-indexed line number.
        #[arg(long)]
        line: Option<usize>,
        /// Heading path element, one per level (repeatable).
        #[arg(long = "heading")]
        heading: Vec<String>,
        #[arg(long)]
        search: String,
        #[arg(long)]
        replace: String,
        /// Bypass the Emacs lockfile guardrail.
        #[arg(long)]
        force: bool,
    },

    /// Insert org-mode text at the specified destination.
    Insert {
        /// Org-mode text to insert verbatim. Adjust heading depth before calling.
        content: String,
        /// Placement: before|after|first-child|last-child|doc-top|doc-bottom.
        #[arg(long)]
        placement: String,
        /// Destination file (always required — no source file to fall back to).
        #[arg(long)]
        dest_file: String,
        /// CUSTOM_ID of the destination anchor section.
        #[arg(long)]
        dest_id: Option<String>,
        /// Line number of the destination anchor section (alternative to --dest-id).
        #[arg(long)]
        dest_line: Option<usize>,
        /// Bypass the Emacs lockfile guardrail.
        #[arg(long)]
        force: bool,
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
        /// Bypass the Emacs lockfile guardrail.
        #[arg(long)]
        force: bool,
    },

    /// Move a section within or between org files.
    Refile {
        /// Source file.
        src_file: String,
        /// :CUSTOM_ID: value of the section to move (preferred).
        #[arg(long = "src-id")]
        src_id: Option<String>,
        /// 0-indexed line number of the section to move.
        #[arg(long = "src-line")]
        src_line: Option<usize>,
        /// Heading path of the section to move, one element per level (repeatable).
        #[arg(long = "src-heading")]
        src_heading: Vec<String>,
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
        /// Bypass the Emacs lockfile guardrail (checked on both src and dest files).
        #[arg(long)]
        force: bool,
    },

    /// Find every link across a directory of org files that points at a file
    /// (optionally narrowed to a specific section within it).
    Backlinks {
        /// File to find inbound links to.
        target_file: String,
        /// Directory to search (recursively).
        root: String,
        /// Narrow to links targeting this :CUSTOM_ID: within target_file.
        #[arg(long)]
        id: Option<String>,
        /// Narrow to links targeting this heading path within target_file,
        /// one element per level (repeatable).
        #[arg(long = "heading")]
        heading: Vec<String>,
    },

    /// Scan a directory of org files for links that fail to resolve.
    CheckLinks {
        /// Directory to search (recursively).
        root: String,
    },
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli.cmd, cli.json) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_output<T: Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", text());
    }
    Ok(())
}

fn run(cmd: Cmd, json: bool) -> Result<()> {
    match cmd {
        Cmd::Outline { file } => {
            let entries: Vec<HeadlineEntry> =
                ops::parse_and_run(&file, |src, tree| org_parser::outline(src, tree))?;
            print_output(json, &entries, || render_outline_text(&entries))?;
        }

        Cmd::Query { path, query, patterns } => {
            let compiled = ops::compile_patterns(&patterns)?;
            let matches = ops::query_path(&path, &query, &compiled)?;
            print_output(json, &matches, || render_query_text(&matches))?;
        }

        Cmd::Subtree { file, id, line, heading } => {
            let r = section_ref_from(None, id, line, heading)?;
            let info = ops::parse_and_run(&file, |src, tree| {
                org_parser::resolve_section_ref(src, tree, &r)
            })?;
            print_output(json, &info, || render_subtree_text(&info, None))?;
        }

        Cmd::OpenLink { link, base } => {
            let target = ops::follow_org_link(&link, base.as_deref())?;
            print_output(json, &target, || render_open_link_text(&target))?;
        }

        Cmd::QueryExamples => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "content": org_parser::QUERY_EXAMPLES,
                    }))?
                );
            } else {
                print!("{}", org_parser::QUERY_EXAMPLES);
            }
        }

        Cmd::PatchSubtree { file, id, line, heading, search, replace, force } => {
            let r = section_ref_from(None, id, line, heading)?;
            let report = ops::run_patch(&file, &r, &search, &replace, force)?;
            print_output(json, &report, || render_patch_text(&report))?;
        }

        Cmd::Insert { content, placement, dest_file, dest_id, dest_line, force } => {
            let dest = build_dest(&placement, Some(dest_file), dest_id, dest_line)?;
            let report = ops::run_insert(&content, &dest, force)?;
            print_output(json, &report, || render_insert_text(&report))?;
        }

        Cmd::EnsureCustomId { file, line, id, force } => {
            let report = ops::run_ensure_custom_id(&file, line, &id, force)?;
            print_output(json, &report, || render_ensure_custom_id_text(&report))?;
        }

        Cmd::Refile {
            src_file, src_id, src_line, src_heading,
            placement, dest_file, dest_id, dest_line, force,
        } => {
            let src = section_ref_from(Some(src_file), src_id, src_line, src_heading)?;
            let dest = build_dest(&placement, dest_file, dest_id, dest_line)?;
            let report = ops::run_refile(&src, &dest, force)?;
            print_output(json, &report, || render_refile_text(&report))?;
        }

        Cmd::Backlinks { target_file, root, id, heading } => {
            let target = BacklinkTarget {
                file: PathBuf::from(&target_file),
                id,
                heading_path: if heading.is_empty() { None } else { Some(heading) },
            };
            let hits = find_backlinks(Path::new(&root), &target)?;
            print_output(json, &hits, || render_backlinks_text(&hits, &target_file, &root))?;
        }

        Cmd::CheckLinks { root } => {
            let errors = check_links(Path::new(&root))?;
            print_output(json, &errors, || render_check_links_text(&errors, &root))?;
        }
    }
    Ok(())
}

// ── arg helpers ───────────────────────────────────────────────────────────────

fn section_ref_from(
    file: Option<String>,
    id: Option<String>,
    line: Option<usize>,
    heading: Vec<String>,
) -> Result<SectionRef> {
    if let Some(id) = id {
        Ok(SectionRef::Id { file, id })
    } else if let Some(n) = line {
        Ok(SectionRef::Line { file, line: n })
    } else if !heading.is_empty() {
        Ok(SectionRef::Path { file, path: heading })
    } else {
        bail!("provide at least one of --id (preferred), --line, or --heading")
    }
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
        "before" => Ok(Dest::Before { section: anchor_ref("before")? }),
        "after" => Ok(Dest::After { section: anchor_ref("after")? }),
        "first-child" => Ok(Dest::FirstChild { section: anchor_ref("first-child")? }),
        "last-child" => Ok(Dest::LastChild { section: anchor_ref("last-child")? }),
        "doc-top" => Ok(Dest::DocTop { file: dest_file }),
        "doc-bottom" => Ok(Dest::DocBottom { file: dest_file }),
        other => bail!(
            "unknown placement {other:?}; use before|after|first-child|last-child|doc-top|doc-bottom"
        ),
    }
}

// ── plain-text renderers ──────────────────────────────────────────────────────
//
// Plain text renders 1-indexed line numbers for human readability; JSON output
// (via print_output's `value` branch) keeps the underlying 0-indexed values
// unchanged — a deliberate, documented asymmetry.

fn render_outline_text(entries: &[HeadlineEntry]) -> String {
    if entries.is_empty() {
        return "(no headlines)".to_string();
    }
    entries
        .iter()
        .map(|e| {
            let stars = "*".repeat(e.depth);
            let kw = e.todo_keyword.as_ref().map(|k| format!("{k} ")).unwrap_or_default();
            let tags = if e.tags.is_empty() {
                String::new()
            } else {
                format!("  :{}:", e.tags.join(":"))
            };
            format!("{stars} {kw}{}{tags}  (row {})", e.title, e.start_position.row + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}

fn render_query_text(matches: &[FileMatch]) -> String {
    if matches.is_empty() {
        return "(no matches)".to_string();
    }
    matches
        .iter()
        .map(|fm| {
            let m = &fm.m;
            let bc = if m.breadcrumbs.is_empty() {
                String::new()
            } else {
                format!("  {}", m.breadcrumbs.join(" > "))
            };
            let first_line = m.text.lines().next().unwrap_or("");
            let mut out = format!(
                "{}:{}  [{}]{bc}\n    text: {first_line}",
                fm.file,
                m.start_position.row + 1,
                m.capture
            );
            if let Some(ctx) = &m.context {
                out.push_str(&format!("\n    context:\n{}", indent(ctx, "      ")));
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_subtree_text(info: &SectionInfo, file: Option<&str>) -> String {
    let mut header = String::new();
    if let Some(f) = file {
        header.push_str(&format!("File: {f}\n"));
    }
    let bc = if info.breadcrumbs.is_empty() {
        String::new()
    } else {
        format!("{} > ", info.breadcrumbs.join(" > "))
    };
    let id_part = info.custom_id.as_ref().map(|id| format!(", id={id}")).unwrap_or_default();
    header.push_str(&format!(
        "=== {bc}{}  (depth {}, row {}{id_part}) ===\n",
        info.title,
        info.depth,
        info.start_line + 1
    ));
    format!("{header}{}", info.subtree)
}

fn render_open_link_text(target: &LinkTarget) -> String {
    match target {
        LinkTarget::Section(s) => render_subtree_text(&s.info, Some(&s.file)),
        LinkTarget::Document(d) => format!("File: {}\n\n{}", d.file, d.content),
    }
}

fn render_patch_text(report: &PatchReport) -> String {
    report.patch.diff.clone()
}

fn render_ensure_custom_id_text(report: &EnsureCustomIdReport) -> String {
    let status = if report.already_existed { "already existed" } else { "inserted" };
    let mut out = format!("CUSTOM_ID: {} ({status})", report.custom_id);
    if let Some(patch) = &report.patch {
        out.push('\n');
        out.push_str(&patch.diff);
    }
    out
}

fn render_warnings(v: &ValidationReport) -> String {
    if v.warnings.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> =
            v.warnings.iter().map(|d| format!("warning: {}", d.message)).collect();
        format!("\n{}", lines.join("\n"))
    }
}

fn render_refile_text(report: &RefileReport) -> String {
    let mut out = format!(
        "Moved \"{}\" from {} to {}:{}",
        report.src.title,
        report.src.file,
        report.dest.file,
        report.dest.line + 1
    );
    let id_part = report.dest.custom_id.as_deref().unwrap_or("<none>");
    out.push_str(&format!("\ncustom_id: {id_part}"));
    if report.custom_id_changed {
        out.push_str(" (renamed to avoid collision)");
    }
    out.push_str(&render_warnings(&report.validation));
    out
}

fn render_insert_text(report: &InsertReport) -> String {
    let mut out = format!("Inserted at {}:{}", report.dest.file, report.dest.line + 1);
    out.push_str(&render_warnings(&report.validation));
    out
}

fn render_backlinks_text(hits: &[BacklinkHit], target_file: &str, root: &str) -> String {
    if hits.is_empty() {
        return format!("no backlinks to {target_file} found under {root}");
    }
    hits.iter()
        .map(|h| {
            let bc = if h.breadcrumbs.is_empty() {
                String::new()
            } else {
                format!("  {}", h.breadcrumbs.join(" > "))
            };
            format!("{}:{}{bc}  {}", h.source_file.display(), h.line + 1, h.link_text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_check_links_text(errors: &[LinkCheckError], root: &str) -> String {
    if errors.is_empty() {
        return format!("no broken links found under {root}");
    }
    errors
        .iter()
        .map(|e| format!("{}:{}  {}  — {}", e.source_file.display(), e.line + 1, e.link_text, e.error))
        .collect::<Vec<_>>()
        .join("\n")
}
