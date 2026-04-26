pub mod parser;
pub mod query;
pub mod types;

pub use parser::make_parser;
pub use query::{
    ensure_custom_id, outline, parse_org_link, patch_subtree, refile_subtree,
    resolve_section_ref, run_query, section_for, validate, EnsureCustomIdResult, OrgLink,
};
pub use types::{
    ByteRange, Dest, Diagnostic, DiagnosticKind, HeadlineEntry, Position, QueryMatch,
    RefileOutput, SectionInfo, SectionRef, ValidationReport,
};

pub const QUERY_EXAMPLES: &str = include_str!("query_examples.md");
