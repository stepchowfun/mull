use crate::{
    error::{Error, SourceRange},
    format::CodeStr,
    line_index::LineIndex,
    scoring::populate_traversal_order,
    wiki::{
        ContentText, FILESYSTEM_LINK_PREFIX, FilesystemTarget, Link, TextNode, Wiki,
        title_line_text, unescaped_characters,
    },
};
use std::{iter, path::Path};

// This struct retains the source information needed to finish a region at its next boundary. A
// region belongs to a node only if its title is valid.
struct PendingNode {
    title: Option<(String, SourceRange)>,
    source_start: usize,
    content_start: usize,
}

// Parse source contents into a scored wiki, with source ranges for every node and link, along with
// any syntax errors. The wiki retains as much of the source as possible so it can still be
// validated and navigated, but it omits the content of duplicate nodes, invalid titles, and
// anything before the first title, so it mustn't be rendered if there are any syntax errors. The
// line index locates syntax errors in their source listings.
pub fn parse(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> (Wiki, Vec<Error>) {
    // Accumulate parsed nodes, errors, and the region currently being read, which starts as the
    // untitled region before the first title.
    let mut wiki = Wiki::default();
    let mut errors = Vec::<Error>::new();
    let mut pending_node = PendingNode {
        title: None,
        source_start: 0,
        content_start: 0,
    };
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

        // Start a new region at each title line.
        if let Some(raw_title) = title_line_text(line) {
            // Treat invalid titles as structural boundaries for subsequent content.
            has_seen_title_marker = true;

            // Finish the preceding region before starting the next one.
            errors.extend(finish_node(
                &mut wiki,
                pending_node,
                line_start,
                source_path,
                source_contents,
                line_index,
            ));

            // Start a region which belongs to a node if the title is valid.
            let title = match parse_title(
                raw_title,
                source_path,
                source_contents,
                line_index,
                line_source_range,
            ) {
                Ok(title) => Some(title),
                Err(error) => {
                    errors.push(error);
                    None
                }
            };
            pending_node = PendingNode {
                title,
                source_start: line_start,
                content_start: next_line_start,
            };
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
                    line_index,
                    trim_source_range(source_contents, line_source_range),
                )),
                None,
                None,
            ));
            reported_content_before_title = true;
        }

        line_start = next_line_start;
    }

    // Finish the final region at the end of the wiki.
    errors.extend(finish_node(
        &mut wiki,
        pending_node,
        source_contents.len(),
        source_path,
        source_contents,
        line_index,
    ));

    // Order the nodes and return the wiki with every error in source order.
    populate_traversal_order(&mut wiki);
    (wiki, errors)
}

// Locate the title that follows a title marker, rejecting titles that are empty after surrounding
// whitespace is stripped, as well as titles that text links couldn't target because they would
// become filesystem links.
fn parse_title(
    raw_title: &str,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    line_source_range: SourceRange,
) -> Result<(String, SourceRange), Error> {
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
            "A node title can't be empty.",
            source_path,
            Some((source_contents, line_index, line_source_range)),
            None,
            None,
        ))
    } else if !title.starts_with(FILESYSTEM_LINK_PREFIX) {
        // Extend the title's range through any trailing whitespace to the end of its line, so the
        // title is found with the cursor anywhere after it.
        Ok((
            title.to_owned(),
            SourceRange {
                start: title_source_range.start,
                end: line_source_range.end,
            },
        ))
    } else {
        Err(Error::new(
            &format!(
                "A node title can't start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ),
            source_path,
            Some((source_contents, line_index, title_source_range)),
            None,
            None,
        ))
    }
}

// Parse a completed region's content, reporting its errors, and add it as a node if it has a valid
// title that hasn't already been used. A node with errors in its content retains the links which
// parsed successfully.
fn finish_node(
    wiki: &mut Wiki,
    pending_node: PendingNode,
    source_end: usize,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    // Locate the trimmed region and its trimmed content in the original source.
    let PendingNode {
        title,
        source_start,
        content_start,
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
    let (content, links, content_errors) = parse_content(
        source_path,
        source_contents,
        line_index,
        content_source_range,
    );

    // Discard the content of a region without a valid title, whose title error was already
    // reported.
    let Some((title, title_source_range)) = title else {
        return content_errors;
    };

    // Reject a title that has already been used, reporting it before the errors in its content.
    if wiki.text_nodes.contains_key(&title) {
        return iter::once(Error::new(
            &format!("Node {} already exists.", title.code_str()),
            source_path,
            Some((source_contents, line_index, title_source_range)),
            None,
            None,
        ))
        .chain(content_errors)
        .collect();
    }

    // Insert the node, retaining the links in its content which parsed successfully.
    wiki.text_nodes.insert(
        title.clone(),
        TextNode {
            title,
            content: ContentText::from_source(&content),
            links,
            traversal_index: None,
            source_range,
            title_source_range,
            has_syntax_errors: !content_errors.is_empty(),
        },
    );
    content_errors
}

// Parse link occurrences and produce the normalized content stored on a text node.
fn parse_content(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
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
    let mut link_has_nested_start = false;

    // Report a syntax error at a range of the source.
    let syntax_error = |message: &str, error_source_range| {
        Error::new(
            message,
            source_path,
            Some((source_contents, line_index, error_source_range)),
            None,
            None,
        )
    };

    for (index, character) in unescaped_characters(original_content) {
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
            errors.push(syntax_error(
                "A link can't contain a line break.",
                link_source_range,
            ));
            link_has_line_break = true;
        }

        // Interpret unescaped square brackets as link delimiters.
        match character {
            '[' if link_start.is_some() => {
                errors.push(syntax_error(
                    "Unexpected opening link delimiter.",
                    character_source_range,
                ));
                link_has_nested_start = true;
            }
            '[' => {
                link_start = Some(index);
                link_has_line_break = false;
                link_has_nested_start = false;
            }
            ']' if link_start.is_none() => {
                // Reject a closing delimiter without an opening delimiter [tag:missing_link_start].
                errors.push(syntax_error(
                    "Unexpected closing link delimiter.",
                    character_source_range,
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

                // Parse the target unless the link has a syntax error, whose target is unreliable.
                let link = if link_has_line_break || link_has_nested_start {
                    None
                } else {
                    Some(parse_link(
                        trimmed_target,
                        source_path,
                        source_contents,
                        line_index,
                        link_source_range,
                    ))
                };

                // Write a filesystem link's target in its canonical form, and keep any other target
                // as written.
                let formatted_target = match &link {
                    Some(Ok(Link::Filesystem { target, .. })) => target.text().into_string(),
                    Some(Ok(Link::Text { .. }) | Err(_)) | None => trimmed_target.to_owned(),
                };
                match link {
                    Some(Ok(link)) => links.push(link),
                    Some(Err(error)) => errors.push(error),
                    None => {}
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
        errors.push(syntax_error("Unclosed link.", link_source_range));
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
    line_index: &LineIndex,
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
                Some((source_contents, line_index, source_range)),
                None,
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
        line_index::LineIndex,
        wiki::{Link, Wiki},
    };
    use std::{
        fmt::Write,
        path::{Path, PathBuf},
    };

    // Parse test sources using a stable path for diagnostic assertions, recovering from syntax
    // errors.
    fn parse_fixture(source_contents: &str) -> (Wiki, Vec<Error>) {
        parse(
            Some(Path::new("test.mull")),
            source_contents,
            &LineIndex::new(source_contents),
        )
    }

    // Parse test sources, rejecting sources with syntax errors.
    fn parse_test(source_contents: &str) -> Result<Wiki, Vec<Error>> {
        let (wiki, errors) = parse_fixture(source_contents);
        if errors.is_empty() {
            Ok(wiki)
        } else {
            Err(errors)
        }
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

    // Parse titles and multiline content while retaining exact source ranges. A title's range
    // extends through trailing whitespace, which the title itself excludes.
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
        assert_eq!(wiki.text_nodes["Home"].title_source_range.end, 13);
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

    // Treat an escaped backslash as a literal character which doesn't escape what follows, and
    // leave other backslashes as is.
    #[test]
    fn escaped_backslashes() {
        let wiki = parse_test(
            r"# Home
See \\[Four], [Five\\], [A\B], and \\\[ignored\].
# Four
# Five\
# A\B",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec!["text:Four", "text:Five\\", "text:A\\B"],
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
            "A link can't contain a line break.",
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
        assert_eq!(
            parse_test("# Empty").unwrap().text_nodes["Empty"]
                .content
                .as_str(),
            "",
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
        assert!(
            errors[0]
                .to_string()
                .contains("A node title can't be empty."),
        );
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
                    .contains("A node title can't start with `/`."),
            );
        }
    }

    // Don't reinterpret content after an invalid title as content before the first title.
    #[test]
    fn content_after_empty_title() {
        let errors = parse_test("# Home\n\nfoo\n\n#\n\nbar").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A node title can't be empty."),
        );
    }

    // Recognize a bare title marker so it can be reported as an empty title.
    #[test]
    fn bare_empty_title() {
        let errors = parse_test("#").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A node title can't be empty."),
        );
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
        assert!(
            errors[1]
                .to_string()
                .contains("A node title can't be empty"),
        );
        assert!(
            errors[2]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[3].to_string().contains("Unclosed link"));
    }

    // Keep a node with a syntax error, along with the links in it which parsed successfully.
    #[test]
    fn recovered_node_keeps_valid_links() {
        let (wiki, errors) = parse_fixture(
            "# Home\nStray] [Greeting] [Bad\nlink] [nested[link] [/../up] [/notes.txt] [unclosed",
        );

        assert_eq!(errors.len(), 5);
        assert!(wiki.text_nodes["Home"].has_syntax_errors);
        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec![
                "text:Greeting".to_owned(),
                format!("file:{}", PathBuf::from("notes.txt").display()),
            ],
        );
    }

    // Score a wiki with syntax errors so its reachability can be validated.
    #[test]
    fn recovered_wiki_has_traversal_order() {
        let (wiki, errors) = parse_fixture("# Home\nStray] [Greeting]\n# Greeting");

        assert_eq!(errors.len(), 1);
        assert_eq!(wiki.text_nodes["Greeting"].traversal_index, Some(1));
        assert!(!wiki.text_nodes["Greeting"].has_syntax_errors);
    }

    // Omit duplicate nodes and regions without valid titles, but report the errors in their
    // content.
    #[test]
    fn recovered_wiki_omits_untitled_regions() {
        let (wiki, errors) = parse_fixture(
            "Before]\n# Home\n[Home]\n# Home\n[Greeting] a]\n#\n[Greeting] b]\n# Greeting",
        );

        assert_eq!(errors.len(), 6);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any node."),
        );
        assert!(errors[1].to_string().contains("1 \u{2502} Before]"));
        assert!(
            errors[2]
                .to_string()
                .contains("Node `Home` already exists."),
        );
        assert!(errors[3].to_string().contains("5 \u{2502} [Greeting] a]"));
        assert!(
            errors[4]
                .to_string()
                .contains("A node title can't be empty."),
        );
        assert!(errors[5].to_string().contains("7 \u{2502} [Greeting] b]"));
        assert_eq!(wiki.text_nodes.len(), 2);
        assert_eq!(
            link_targets(&wiki.text_nodes["Home"].links),
            vec!["text:Home"],
        );
        assert_eq!(wiki.text_nodes["Greeting"].traversal_index, None);
    }
}
