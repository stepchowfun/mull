use crate::{error::Error, format::CodeStr, parser, validator, wiki::Wiki};
use similar::TextDiff;
use std::path::Path;

// Parse and validate source contents with any available surrounding filesystem.
pub fn analyze(source_path: Option<&Path>, source_contents: &str) -> Result<Wiki, Vec<Error>> {
    // Parse and score the wiki before performing validations that require its structure.
    let wiki = parser::parse(source_path, source_contents)?;
    validator::validate(&wiki, source_path, source_contents)?;
    Ok(wiki)
}

// Perform every check required by the check command while retaining the parsed wiki.
pub fn check(source_path: Option<&Path>, source_contents: &str) -> Result<Wiki, Vec<Error>> {
    // Analyze the source before comparing it with its canonical rendering.
    let wiki = analyze(source_path, source_contents)?;
    let rendered_wiki = wiki.to_string();
    if source_contents != rendered_wiki {
        let diff = TextDiff::from_lines(source_contents, &rendered_wiki)
            .unified_diff()
            .header("wiki", "rendered")
            .to_string();
        return Err(vec![Error::new(
            &format!(
                "The wiki is not formatted correctly. {} can fix it.\n\n{diff}",
                "mull fix".code_str(),
            ),
            source_path,
            None,
            None,
        )]);
    }

    // Every check succeeded.
    Ok(wiki)
}
