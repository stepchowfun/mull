use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::Error,
    parser, validator,
    wiki::Wiki,
};
use std::path::Path;

// Parse and validate source contents with any available surrounding filesystem.
pub fn analyze(
    source_path: Option<&Path>,
    source_contents: &str,
    cancellation: &CancellationFlag,
) -> Outcome<Result<Wiki, Vec<Error>>> {
    // Parse and score the wiki before performing validations that require its structure.
    let wiki = match parser::parse(source_path, source_contents) {
        Ok(wiki) => wiki,
        Err(errors) => return Outcome::Completed(Err(errors)),
    };
    validator::validate(&wiki, source_path, source_contents, cancellation)
        .map(|result| result.map(|()| wiki))
}
