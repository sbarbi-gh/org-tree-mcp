use anyhow::{Context, Result};
use tree_sitter::Parser;

pub fn make_parser() -> Result<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_org::language())
        .context("failed to load org grammar")?;
    Ok(parser)
}
