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
    // Parse and score as much of the wiki as possible, then validate it even if it has syntax
    // errors, so they don't hide validation errors. The wiki is returned only if it has neither,
    // since a wiki with syntax errors doesn't represent all of its source.
    let (wiki, mut errors) = parser::parse(source_path, source_contents);
    validator::validate(&wiki, source_path, source_contents, cancellation).map(|result| {
        if let Err(validation_errors) = result {
            errors.extend(validation_errors);
        }
        if errors.is_empty() {
            Ok(wiki)
        } else {
            Err(errors)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::analyze;
    use crate::{cancellation::CancellationFlag, error::Error, wiki::Wiki};

    // Analyze an untitled wiki, which nothing cancels.
    fn analyze_test(source_contents: &str) -> Result<Wiki, Vec<Error>> {
        analyze(None, source_contents, &CancellationFlag::default()).assume_completed()
    }

    // Report validation errors alongside syntax errors, after them.
    #[test]
    fn validation_errors_accompany_syntax_errors() {
        let errors =
            analyze_test("# Home\nSee [Missing] and [Greeting].\n# Greeting\nUnexpected].")
                .unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter."),
        );
        assert!(errors[1].to_string().contains("Node `Missing` not found."));
    }

    // Keep a node with a syntax error from making the nodes it links to unreachable.
    #[test]
    fn syntax_errors_preserve_reachability() {
        let errors =
            analyze_test("# Home\nSee [Greeting].\n# Greeting\nStray] and [Farewell].\n# Farewell")
                .unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter."),
        );
    }

    // Don't validate the target of a link with a syntax error.
    #[test]
    fn malformed_links_are_not_validated() {
        let errors = analyze_test("# Home\nSee [Missing[Home].").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected opening link delimiter."),
        );
    }

    // Accept a valid wiki.
    #[test]
    fn valid_wiki() {
        assert!(analyze_test("# Home\nSee [Greeting].\n# Greeting").is_ok());
    }
}
