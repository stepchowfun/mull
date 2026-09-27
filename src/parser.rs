use crate::{
    error::{Error, SourceRange},
    format::CodeStr,
    scoring::populate_depths,
    wiki::{
        ContentText, FILESYSTEM_LINK_PREFIX, FilesystemTarget, Link, TITLE_MARKER, TITLE_PREFIX,
        TextNode, Wiki,
    },
};
use std::path::Path;

// This struct retains the source information needed to finish a node at its next boundary.
struct PendingNode {
    title: String,
    source_start: usize,
    content_start: usize,
    title_source_range: SourceRange,
}

// Parse source contents into a scored wiki with source ranges for every node and link.
pub fn parse(source_path: Option<&Path>, source_contents: &str) -> Result<Wiki, Vec<Error>> {
    // Accumulate parsed nodes, errors, and the node currently being read.
    let mut wiki = Wiki::default();
    let mut errors = Vec::<Error>::new();
    let mut pending_node = None::<PendingNode>;
    let mut has_seen_title_marker = false;
    let mut reported_content_before_title = false;
    let mut line_start = 0;

    // Process ranged source lines as boundaries while retaining their original byte offsets.
    for raw_line in source_contents.split_inclusive('\n') {
        let next_line_start = line_start + raw_line.len();
        let line_with_possible_carriage_return = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line_with_possible_carriage_return
            .strip_suffix('\r')
            .unwrap_or(line_with_possible_carriage_return);
        let line_source_range = SourceRange {
            start: line_start,
            end: line_start + line.len(),
        };

        // Recognize a title marker followed by either a space or the end of the line.
        let raw_title = if line == TITLE_MARKER {
            Some("")
        } else {
            line.strip_prefix(TITLE_PREFIX)
        };
        if let Some(raw_title) = raw_title {
            // Treat invalid titles as structural boundaries for subsequent content.
            has_seen_title_marker = true;

            // Finish the preceding node before starting the next one.
            if let Some(previous_node) = pending_node.take()
                && let Err(node_errors) = insert_node(
                    &mut wiki,
                    previous_node,
                    line_start,
                    source_path,
                    source_contents,
                )
            {
                errors.extend(node_errors);
            }

            // Start a node for the title if it's valid.
            match parse_title(raw_title, source_path, source_contents, line_source_range) {
                Ok(title_source_range) => {
                    pending_node = Some(PendingNode {
                        title: source_contents[title_source_range.start..title_source_range.end]
                            .to_owned(),
                        source_start: line_start,
                        content_start: next_line_start,
                        title_source_range,
                    });
                }
                Err(error) => errors.push(error),
            }
        } else if !has_seen_title_marker
            && !reported_content_before_title
            && !line.trim().is_empty()
        {
            // Report only the first non-whitespace content outside a valid node.
            errors.push(Error::new(
                "This content isn't in any node.",
                source_path,
                Some((
                    source_contents,
                    trim_source_range(source_contents, line_source_range),
                )),
                None,
            ));
            reported_content_before_title = true;
        }

        line_start = next_line_start;
    }

    // Finish the final node at the end of the wiki.
    if let Some(final_node) = pending_node
        && let Err(node_errors) = insert_node(
            &mut wiki,
            final_node,
            source_contents.len(),
            source_path,
            source_contents,
        )
    {
        errors.extend(node_errors);
    }

    // Return all node errors together, or score and return the parsed wiki.
    if errors.is_empty() {
        populate_depths(&mut wiki);
        Ok(wiki)
    } else {
        Err(errors)
    }
}

// Locate the title that follows a title marker, rejecting titles that are empty after surrounding
// whitespace is stripped, as well as titles that text links couldn't target because they would
// become filesystem links.
fn parse_title(
    raw_title: &str,
    source_path: Option<&Path>,
    source_contents: &str,
    line_source_range: SourceRange,
) -> Result<SourceRange, Error> {
    // Strip surrounding whitespace from the text after the marker, which ends the line.
    let title_source_range = trim_source_range(
        source_contents,
        SourceRange {
            start: line_source_range.end - raw_title.len(),
            end: line_source_range.end,
        },
    );
    let title = &source_contents[title_source_range.start..title_source_range.end];

    // Report an empty title at the whole line, since the title has no text of its own.
    if title.is_empty() {
        Err(Error::new(
            "This title can't be empty.",
            source_path,
            Some((source_contents, line_source_range)),
            None,
        ))
    } else if !title.starts_with(FILESYSTEM_LINK_PREFIX) {
        Ok(title_source_range)
    } else {
        Err(Error::new(
            &format!(
                "This title can't start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ),
            source_path,
            Some((source_contents, title_source_range)),
            None,
        ))
    }
}

// Add a completed node after collecting all of its parsing errors.
fn insert_node(
    wiki: &mut Wiki,
    pending_node: PendingNode,
    source_end: usize,
    source_path: Option<&Path>,
    source_contents: &str,
) -> Result<(), Vec<Error>> {
    // Locate the trimmed node and its trimmed content in the original source.
    let PendingNode {
        title,
        source_start,
        content_start,
        title_source_range,
    } = pending_node;
    let source_range = trim_source_range(
        source_contents,
        SourceRange {
            start: source_start,
            end: source_end,
        },
    );
    let content_source_range = trim_source_range(
        source_contents,
        SourceRange {
            start: content_start,
            end: source_end,
        },
    );
    let (content, links, mut errors) =
        parse_content(source_path, source_contents, content_source_range);

    // Reject a title that has already been used.
    if wiki.text_nodes.contains_key(&title) {
        errors.push(Error::new(
            &format!("Node {} already exists.", title.code_str()),
            source_path,
            Some((source_contents, title_source_range)),
            None,
        ));
    }

    // Insert only nodes that parsed without errors.
    if errors.is_empty() {
        wiki.text_nodes.insert(
            title.clone(),
            TextNode {
                title,
                content: ContentText::from_source(&content),
                links,
                depth: None,
                source_range,
                title_source_range,
            },
        );
        Ok(())
    } else {
        Err(errors)
    }
}

// Parse link occurrences and produce the normalized content stored on a text node.
fn parse_content(
    source_path: Option<&Path>,
    source_contents: &str,
    source_range: SourceRange,
) -> (String, Vec<Link>, Vec<Error>) {
    // Traverse the original content while retaining indices for copying and diagnostics.
    let original_content = &source_contents[source_range.start..source_range.end];
    let mut content = String::new();
    let mut copied_through = 0;
    let mut links = Vec::<Link>::new();
    let mut errors = Vec::<Error>::new();
    let mut link_start = None::<usize>;
    let mut link_has_line_break = false;
    let mut previous_was_backslash = false;

    for (index, character) in original_content.char_indices() {
        let is_escaped_delimiter = previous_was_backslash && matches!(character, '[' | ']');
        previous_was_backslash = character == '\\';
        if is_escaped_delimiter {
            continue;
        }

        // Locate the current character for any delimiter error.
        let character_source_range = SourceRange {
            start: source_range.start + index,
            end: source_range.start + index + character.len_utf8(),
        };

        // Report the first line break within each link.
        if character == '\n'
            && let Some(start) = link_start
            && !link_has_line_break
        {
            let link_source_range = SourceRange {
                start: source_range.start + start,
                end: source_range.start + index + character.len_utf8(),
            };
            errors.push(Error::new(
                "This link contains a line break.",
                source_path,
                Some((source_contents, link_source_range)),
                None,
            ));
            link_has_line_break = true;
        }

        // Interpret unescaped square brackets as link delimiters.
        match character {
            '[' if link_start.is_some() => errors.push(Error::new(
                "Unexpected opening link delimiter.",
                source_path,
                Some((source_contents, character_source_range)),
                None,
            )),
            '[' => {
                link_start = Some(index);
                link_has_line_break = false;
            }
            ']' if link_start.is_none() => {
                // Reject a closing delimiter without an opening delimiter [tag:missing_link_start].
                errors.push(Error::new(
                    "Unexpected closing link delimiter.",
                    source_path,
                    Some((source_contents, character_source_range)),
                    None,
                ));
            }
            ']' => {
                // The preceding arm rejected a missing link start [ref:missing_link_start].
                let start = link_start.take().expect("The link start was checked.");
                let inner_start = start + '['.len_utf8();
                let trimmed_target = original_content[inner_start..index].trim();
                let link_source_range = SourceRange {
                    start: source_range.start + start,
                    end: source_range.start + index + character.len_utf8(),
                };
                let link = parse_link(
                    trimmed_target,
                    source_path,
                    source_contents,
                    link_source_range,
                );

                // Write a filesystem link's target in its canonical form, and keep any other target
                // as written.
                let formatted_target = match &link {
                    Ok(Link::Filesystem { target, .. }) => target.text().into_string(),
                    Ok(Link::Text { .. }) | Err(_) => trimmed_target.to_owned(),
                };
                match link {
                    Ok(link) => links.push(link),
                    Err(error) => errors.push(error),
                }

                // Copy the prose before the link, then the link in its formatted form.
                content.push_str(&original_content[copied_through..inner_start]);
                content.push_str(&formatted_target);
                content.push(']');
                copied_through = index + character.len_utf8();
            }
            _ => {}
        }
    }

    // Reject an opening delimiter that has no closing delimiter.
    if let Some(start) = link_start {
        let link_source_range = SourceRange {
            start: source_range.start + start,
            end: source_range.start + start + '['.len_utf8(),
        };
        errors.push(Error::new(
            "Unclosed link.",
            source_path,
            Some((source_contents, link_source_range)),
            None,
        ));
    }

    // Retain the content following the final link, then normalize the finished content. Links
    // can't contain line breaks, and each ends with a delimiter, so normalizing never changes one.
    content.push_str(&original_content[copied_through..]);
    (normalize_lines(&content), links, errors)
}

// Convert the contents of a closed delimiter pair into a typed link occurrence.
fn parse_link(
    target: &str,
    source_path: Option<&Path>,
    source_contents: &str,
    source_range: SourceRange,
) -> Result<Link, Error> {
    // Parse a filesystem link's target, which starts with `/`, and unescape any other link's title.
    // The target is part of a node's content, as written.
    let target = ContentText::from_source(target);
    if target.as_str().starts_with(FILESYSTEM_LINK_PREFIX) {
        let target = FilesystemTarget::parse(&target).map_err(|message| {
            Error::new(
                &message,
                source_path,
                Some((source_contents, source_range)),
                None,
            )
        })?;
        Ok(Link::Filesystem {
            target,
            source_range,
        })
    } else {
        Ok(Link::Text {
            title: target.unescape(),
            source_range,
        })
    }
}

// Write text in the wiki's canonical form, with Unix line endings and no whitespace at the end of a
// line.
fn normalize_lines(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

// Remove surrounding whitespace from a source range without losing its original coordinates.
fn trim_source_range(source_contents: &str, source_range: SourceRange) -> SourceRange {
    // Find the trimmed start relative to the original source.
    let source = &source_contents[source_range.start..source_range.end];
    let start_trimmed = source.trim_start();
    let start = source_range.start + source.len() - start_trimmed.len();

    // Find the trimmed end relative to the adjusted start.
    let trimmed = start_trimmed.trim_end();
    SourceRange {
        start,
        end: start + trimmed.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::{
        assert_fails,
        error::Error,
        wiki::{Link, Wiki},
    };
    use std::{
        fmt::Write,
        path::{Path, PathBuf},
    };

    // Parse test sources using a stable path for diagnostic assertions.
    fn parse_test(source_contents: &str) -> Result<Wiki, Vec<Error>> {
        parse(Some(Path::new("test.mull")), source_contents)
    }

    // Describe link targets without coupling semantic assertions to their source ranges.
    fn link_targets(links: &[Link]) -> Vec<String> {
        links
            .iter()
            .map(|link| match link {
                Link::Text { title, .. } => format!("text:{title}"),
                Link::Filesystem { target, .. } => format!(
                    "{}:{}",
                    if target.is_directory() { "dir" } else { "file" },
                    target.path().display(),
                ),
            })
            .collect()
    }

    // Parse titles and multiline content while retaining exact source ranges.
    #[test]
    fn nodes() {
        let source =
            "  \n#   Home  \n\n Check out the [Greeting]. \n\n# Greeting\n Hello,\nworld! \n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.text_nodes.len(), 2);
        assert_eq!(wiki.text_nodes["Home"].title, "Home");
        assert_eq!(
            wiki.text_nodes["Home"].content.as_str(),
            "Check out the [Greeting].",
        );
        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec!["text:Greeting"],
        );
        assert_eq!(
            wiki.text_nodes["Greeting"].content.as_str(),
            "Hello,\nworld!",
        );
        assert_eq!(wiki.text_nodes["Home"].title_source_range.start, 7);
        assert_eq!(wiki.text_nodes["Home"].title_source_range.end, 11);
        assert_eq!(wiki.text_nodes["Home"].source_range.start, 3);
        assert_eq!(wiki.text_nodes["Home"].source_range.end, 41);
        let Link::Text { source_range, .. } = &wiki.text_nodes["Home"].links[0] else {
            panic!("The parsed link should be a text link.");
        };
        assert_eq!(&source[source_range.start..source_range.end], "[Greeting]");
    }

    // Preserve duplicate links as distinct source occurrences while normalizing their text.
    #[test]
    fn links() {
        let wiki = parse_test(concat!(
            "# Home\nSee [Greeting], [ About ], and [Greeting].",
            "\n# About\n# Greeting",
        ))
        .unwrap();

        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec!["text:Greeting", "text:About", "text:Greeting"],
        );
        assert_eq!(
            wiki.text_nodes["Home"].content.as_str(),
            "See [Greeting], [About], and [Greeting].",
        );
    }

    // Keep source ranges on UTF-8 boundaries when titles and links contain multibyte characters.
    #[test]
    fn unicode_source_ranges() {
        let source = "# Home\nSee [Grüße].\n# Grüße";
        let wiki = parse_test(source).unwrap();
        let Link::Text { source_range, .. } = &wiki.text_nodes["Home"].links[0] else {
            panic!("The parsed link should be a text link.");
        };

        assert_eq!(&source[source_range.start..source_range.end], "[Grüße]");
        assert_eq!(
            &source[wiki.text_nodes["Grüße"].title_source_range.start
                ..wiki.text_nodes["Grüße"].title_source_range.end],
            "Grüße",
        );
    }

    // Treat escaped square brackets as literal link-title characters.
    #[test]
    fn escaped_link_delimiters() {
        let wiki = parse_test(
            r"# Home
See \[Ignored\], [One\]Two], [\[Three], [Four], and \[also ignored\].
# Four
# One]Two
# [Three",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec!["text:One]Two", "text:[Three", "text:Four"],
        );
    }

    // Parse file and directory links separately from text links.
    #[test]
    fn filesystem_links() {
        let wiki = parse_test(
            "# Home\nSee [/notes.txt], [/images/], [/images/./raw/], [/], [//docs/], and \
                [/ spaced.txt].",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec![
                format!("file:{}", PathBuf::from("notes.txt").display()),
                format!("dir:{}", PathBuf::from("images").display()),
                format!("dir:{}", PathBuf::from("images").join("raw").display()),
                "dir:".to_owned(),
                format!("dir:{}", PathBuf::from("docs").display()),
                format!("file:{}", PathBuf::from(" spaced.txt").display()),
            ],
        );
    }

    // Format links by trimming their targets and normalizing filesystem paths, writing a directory
    // with a trailing `/` and the wiki directory as `/`.
    #[test]
    fn formatted_links() {
        let wiki = parse_test(
            "# Home\n[ Home ] [ /a//b/./c\\[1\\].txt ] [/images/] [/images/./raw//] [/] [/./] \
                [///]",
        )
        .unwrap();

        assert_eq!(
            wiki.text_nodes["Home"].content.as_str(),
            "[Home] [/a/b/c\\[1\\].txt] [/images/] [/images/raw/] [/] [/] [/]",
        );
    }

    // Remove whitespace at the end of each line, including after a link and on blank lines.
    #[test]
    fn trailing_whitespace() {
        let wiki =
            parse_test("# Home\r\nFirst  \t\r\n   \nSee [Home]  \nand [Home] later.\t\nLast")
                .unwrap();

        assert_eq!(
            wiki.text_nodes["Home"].content.as_str(),
            "First\n\nSee [Home]\nand [Home] later.\nLast",
        );
    }

    // Reject filesystem links whose paths contain parent components.
    #[test]
    fn invalid_filesystem_link_paths() {
        let result = parse_test("# Home\n[/../notes.txt] [/notes/../notes.txt]");

        assert_fails!(result.clone(), "Path `../notes.txt` must not contain `..`.");
        assert_fails!(result, "Path `notes/../notes.txt` must not contain `..`.");
    }

    // Reject opening link delimiters that aren't closed.
    #[test]
    fn unclosed_link() {
        assert_fails!(parse_test("# Home\nSee [Greeting."), "Unclosed link.");
    }

    // Reject links that span multiple lines.
    #[test]
    fn link_with_line_break() {
        assert_fails!(
            parse_test("# Home\nSee [Greeting\ncontinued]."),
            "This link contains a line break.",
        );
    }

    // Reject unescaped opening delimiters inside links.
    #[test]
    fn unexpected_opening_delimiter() {
        assert_fails!(
            parse_test("# Home\nSee [nested[Greeting]."),
            "Unexpected opening link delimiter.",
        );
    }

    // Reject unescaped closing delimiters outside links.
    #[test]
    fn unexpected_closing_delimiter() {
        let errors = parse_test("# Home\nSee Greeting].").unwrap_err();

        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[0].to_string().contains("2 \u{2502} See Greeting]."));
    }

    // Treat non-title hash prefixes as ordinary content.
    #[test]
    fn non_title_hashes() {
        let wiki = parse_test("# Home\n\n## Subtitle\n#not a title").unwrap();

        assert_eq!(
            wiki.text_nodes["Home"].content.as_str(),
            "## Subtitle\n#not a title",
        );
    }

    // Accept empty and whitespace-only wikis.
    #[test]
    fn empty_wiki() {
        assert!(parse_test(" \n\t\n").unwrap().text_nodes.is_empty());
    }

    // Accept empty node content.
    #[test]
    fn empty_content() {
        assert!(
            parse_test("# Empty").unwrap().text_nodes["Empty"]
                .content
                .as_str()
                .is_empty(),
        );
    }

    // Parse Windows line endings without retaining carriage returns.
    #[test]
    fn windows_line_endings() {
        let wiki = parse_test("# Greeting\r\n\r\nHello, world!\r\n").unwrap();

        assert_eq!(
            wiki.text_nodes["Greeting"].content.as_str(),
            "Hello, world!",
        );
    }

    // Reject non-whitespace content before the first title.
    #[test]
    fn content_before_title() {
        assert_fails!(
            parse_test("Introduction\n# Home"),
            "This content isn't in any node.",
        );
    }

    // Reject titles that are empty after whitespace is stripped.
    #[test]
    fn empty_title() {
        let errors = parse_test("#   \nContent").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("This title can't be empty."));
    }

    // Reject titles that text links would interpret as filesystem links.
    #[test]
    fn filesystem_link_titles() {
        let errors = parse_test("# Home\n\n# /notes.txt\n\n# /\n\n# notes: /draft").unwrap_err();

        assert_eq!(errors.len(), 2);
        for error in &errors {
            assert!(
                error
                    .to_string()
                    .contains("This title can't start with `/`."),
            );
        }
    }

    // Don't reinterpret content after an invalid title as content before the first title.
    #[test]
    fn content_after_empty_title() {
        let errors = parse_test("# Home\n\nfoo\n\n#\n\nbar").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("This title can't be empty."));
    }

    // Recognize a bare title marker so it can be reported as an empty title.
    #[test]
    fn bare_empty_title() {
        let errors = parse_test("#").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("This title can't be empty."));
        assert!(errors[0].to_string().contains("1 │ #"));
    }

    // Report only the first occurrence of content before a valid title.
    #[test]
    fn repeated_content_before_title() {
        let errors = parse_test("First\nSecond\n# Home").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any node."),
        );
    }

    // Reject duplicate titles and identify the later declaration.
    #[test]
    fn duplicate_title() {
        let errors = parse_test("# Home\nFirst\n# Home\nSecond").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Node `Home` already exists."),
        );
        assert!(errors[0].to_string().contains("3 \u{2502} # Home"));
    }

    // Report errors from every invalid node in source order.
    #[test]
    fn multiple_node_errors() {
        let errors = parse_test("# First\nUnexpected].\n# Second\nUnclosed [link.").unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[1].to_string().contains("Unclosed link"));
    }

    // Report every delimiter error within one node.
    #[test]
    fn multiple_errors_in_node() {
        let errors = parse_test("# Home\nUnexpected] and [nested[link.").unwrap_err();

        assert_eq!(errors.len(), 3);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(
            errors[1]
                .to_string()
                .contains("Unexpected opening link delimiter"),
        );
        assert!(errors[2].to_string().contains("Unclosed link"));
    }

    // Report structural and node errors together in source order.
    #[test]
    fn multiple_error_types() {
        let errors = parse_test(concat!(
            "Introduction\n",
            "#   \n",
            "Content\n",
            "# First\n",
            "Unexpected].\n",
            "# Second\n",
            "Unclosed [link.",
        ))
        .unwrap_err();

        assert_eq!(errors.len(), 4);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any node"),
        );
        assert!(errors[1].to_string().contains("This title can't be empty"));
        assert!(
            errors[2]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[3].to_string().contains("Unclosed link"));
    }
}
