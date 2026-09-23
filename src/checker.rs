use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::Error,
    format::CodeStr,
    parser, validator,
    wiki::Wiki,
};
use similar::TextDiff;
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

// Perform every check required by the check command while retaining the parsed wiki.
pub fn check(
    source_path: Option<&Path>,
    source_contents: &str,
    cancellation: &CancellationFlag,
) -> Outcome<Result<Wiki, Vec<Error>>> {
    // Analyze the source before comparing it with its canonical rendering.
    let wiki = match analyze(source_path, source_contents, cancellation) {
        Outcome::Completed(Ok(wiki)) => wiki,
        Outcome::Completed(Err(errors)) => return Outcome::Completed(Err(errors)),
        Outcome::Cancelled => return Outcome::Cancelled,
    };
    let rendered_wiki = wiki.to_string();
    if source_contents != rendered_wiki {
        let diff = TextDiff::from_lines(source_contents, &rendered_wiki)
            .unified_diff()
            .header("wiki", "rendered")
            .to_string();
        return Outcome::Completed(Err(vec![Error::new(
            &format!(
                "The wiki is not formatted correctly. {} can fix it.\n\n{diff}",
                "mull fix".code_str(),
            ),
            source_path,
            None,
            None,
        )]));
    }

    // Every check succeeded.
    Outcome::Completed(Ok(wiki))
}
