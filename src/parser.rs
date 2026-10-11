use crate::{
    code_spans::CodeSpans,
    error::{Error, Fix, SourceRange},
    escaping::{Context, invalid_escape_message, invalid_escapes, unescape, unescaped_characters},
    format::CodeStr,
    line_index::LineIndex,
    scoring::populate_traversal_order,
    wiki::{
        FILESYSTEM_LINK_PREFIX, FilesystemTarget, Link, Page, PageTitle, Wiki, title_line_text,
    },
};
use std::{iter, path::Path};

// This struct retains the source information needed to finish a region at its next boundary. A
// region belongs to a page only if its title is valid.
struct PendingPage {
    title: Option<(PageTitle, SourceRange)>,
    source_start: usize,
    content_start: usize,
}

// Parse source contents into a scored wiki, with source ranges for every page and link, along with
// any syntax errors. The wiki retains as much of the source as possible so it can still be
// validated and navigated, but it omits the content of duplicate pages, invalid titles, and
// anything before the first title, so it mustn't be rendered if there are any syntax errors. The
// line index locates syntax errors in their source listings.
pub fn parse(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> (Wiki, Vec<Error>) {
    // Accumulate parsed pages, errors, and the region currently being read, which starts as the
    // untitled region before the first title.
    let mut wiki = Wiki::new(source_contents);
    let mut errors = Vec::<Error>::new();
    let mut pending_page = PendingPage {
        title: None,
        source_start: 0,
        content_start: 0,
    };
    let mut has_seen_title_marker = false;
    let mut reported_content_before_title = false;
    let mut line_start = 0;

    // Find the code spans, whose lines can't be titles, and report each run of backticks which
    // isn't closed when its line is reached, so errors stay in source order.
    let code_spans = CodeSpans::new(source_contents);
    let mut unclosed_runs = code_spans.unclosed().iter().peekable();

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

        // Report each run of backticks on the line which isn't closed.
        while let Some(run_source_range) = unclosed_runs.next_if(|run| run.start < next_line_start)
        {
            errors.push(Error::new(
                "Unclosed code.",
                source_path,
                Some((source_contents, line_index, *run_source_range)),
                None,
                None,
            ));
        }

        // Start a new region at each title line outside a code span.
        if let Some(raw_title) = title_line_text(line).filter(|_| !code_spans.contains(line_start))
        {
            // Treat invalid titles as structural boundaries for subsequent content.
            has_seen_title_marker = true;

            // Finish the preceding region before starting the next one.
            errors.extend(finish_page(
                &mut wiki,
                pending_page,
                line_start,
                source_path,
                source_contents,
                line_index,
                &code_spans,
            ));

            // Start a region which belongs to a page if the title is valid.
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
            pending_page = PendingPage {
                title,
                source_start: line_start,
                content_start: next_line_start,
            };
        } else if !has_seen_title_marker
            && !reported_content_before_title
            && !line.trim().is_empty()
        {
            // Report only the first non-whitespace content outside a valid page.
            errors.push(Error::new(
                "This content isn't in any page.",
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
    errors.extend(finish_page(
        &mut wiki,
        pending_page,
        source_contents.len(),
        source_path,
        source_contents,
        line_index,
        &code_spans,
    ));

    // Order the pages and return the wiki with every error in source order.
    populate_traversal_order(&mut wiki);
    (wiki, errors)
}

// Locate the title that follows a title marker, rejecting a title which breaks the rules for
// titles, such as one which is empty once surrounding whitespace is stripped.
fn parse_title(
    raw_title: &str,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    line_source_range: SourceRange,
) -> Result<(PageTitle, SourceRange), Error> {
    // Strip surrounding whitespace from the text after the marker, which ends the line.
    let title_source_range = trim_source_range(
        source_contents,
        SourceRange {
            start: line_source_range.end - raw_title.len(),
            end: line_source_range.end,
        },
    );

    // Extend a title's range through any trailing whitespace to the end of its line, so the title
    // is found with the cursor anywhere after it. Report a problem at the title, or at the whole
    // line if the title is empty, since it then has no text of its own.
    match PageTitle::parse(raw_title) {
        Ok(title) => Ok((
            title,
            SourceRange {
                start: title_source_range.start,
                end: line_source_range.end,
            },
        )),
        Err(message) => Err(Error::new(
            &message,
            source_path,
            Some((
                source_contents,
                line_index,
                if title_source_range.start == title_source_range.end {
                    line_source_range
                } else {
                    title_source_range
                },
            )),
            None,
            None,
        )),
    }
}

// Parse a completed region's content, reporting its errors, and add it as a page if it has a valid
// title that hasn't already been used. A page with errors in its content retains the links which
// parsed successfully.
fn finish_page(
    wiki: &mut Wiki,
    pending_page: PendingPage,
    source_end: usize,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    code_spans: &CodeSpans,
) -> Vec<Error> {
    // Locate the trimmed region and its content in the original source.
    let PendingPage {
        title,
        source_start,
        content_start,
    } = pending_page;
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

    // Parse the content's links.
    let (links, content_errors) = parse_content(
        source_path,
        source_contents,
        line_index,
        content_source_range,
        code_spans,
    );

    // Discard the content of a region without a valid title, whose title error was already
    // reported.
    let Some((title, title_source_range)) = title else {
        return content_errors;
    };

    // Reject a title that has already been used, reporting it before the errors in its content.
    if wiki.pages.contains_key(&title) {
        return iter::once(Error::new(
            &format!("Page {} already exists.", title.code_str()),
            source_path,
            Some((source_contents, line_index, title_source_range)),
            None,
            None,
        ))
        .chain(content_errors)
        .collect();
    }

    // Insert the page, retaining the links in its content which parsed successfully.
    wiki.pages.insert(
        title.clone(),
        Page {
            title,
            links,
            traversal_index: None,
            source_range,
            title_source_range,
        },
    );
    content_errors
}

// Parse the link occurrences in a page's content, reporting any syntax errors. Completion finds the
// link containing the cursor on its own. Keep this in sync with [group:link_delimiters].
#[allow(
    clippy::too_many_lines,
    reason = "The function scans content once, tracking the state of the link being read."
)]
fn parse_content(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    source_range: SourceRange,
    code_spans: &CodeSpans,
) -> (Vec<Link>, Vec<Error>) {
    // Traverse the original content while retaining indices for diagnostics.
    let original_content = &source_contents[source_range.start..source_range.end];
    let mut links = Vec::<Link>::new();
    let mut errors = Vec::<Error>::new();
    let mut link_start = None::<usize>;
    let mut link_has_nested_start = false;
    let mut link_targets = Vec::<SourceRange>::new(); // Relative to the content, in order

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

    // Report a link that isn't closed by the end of its line at its opening delimiter.
    let unclosed_link = |start: usize| {
        syntax_error(
            "Unclosed link.",
            SourceRange {
                start: source_range.start + start,
                end: source_range.start + start + '['.len_utf8(),
            },
        )
    };

    for (index, character) in unescaped_characters(original_content) {
        // Take the text of code spans as written, so their brackets don't delimit links. A span
        // can't start within a link, so skipping one never skips a link's closing delimiter.
        if code_spans.contains(source_range.start + index) {
            continue;
        }

        // Locate the current character for any delimiter error.
        let character_source_range = SourceRange {
            start: source_range.start + index,
            end: source_range.start + index + character.len_utf8(),
        };

        // Interpret unescaped square brackets as link delimiters, and line breaks as the ends of
        // links.
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
                link_targets.push(SourceRange {
                    start: inner_start,
                    end: index,
                });
                let trimmed_target = original_content[inner_start..index].trim();
                let link_source_range = SourceRange {
                    start: source_range.start + start,
                    end: source_range.start + index + character.len_utf8(),
                };

                // Parse the target unless the link has a syntax error, whose target is unreliable,
                // including a backslash which doesn't escape anything.
                let link = if link_has_nested_start {
                    None
                } else {
                    unescape(trimmed_target, Context::Link).map(|unescaped_target| {
                        parse_link(
                            &unescaped_target,
                            source_path,
                            source_contents,
                            line_index,
                            link_source_range,
                        )
                    })
                };

                // Keep the link if it parsed, and report it otherwise.
                match link {
                    Some(Ok(link)) => links.push(link),
                    Some(Err(error)) => errors.push(error),
                    None => {}
                }
            }
            '\n' => {
                // End an unclosed link at the end of its line, since links can't contain line
                // breaks, so the following lines are read as usual.
                if let Some(start) = link_start.take() {
                    link_targets.push(SourceRange {
                        start: start + '['.len_utf8(),
                        end: index,
                    });
                    errors.push(unclosed_link(start));
                }
            }
            _ => {}
        }
    }

    // Reject a link that's still open at the end of the content.
    if let Some(start) = link_start {
        link_targets.push(SourceRange {
            start: start + '['.len_utf8(),
            end: original_content.len(),
        });
        errors.push(unclosed_link(start));
    }

    // Find each backslash which doesn't escape anything in its context: within a link's target, or
    // in the text around links outside code spans. An escape can't straddle a link's delimiters,
    // since they aren't escaped.
    let mut invalid_escape_offsets = link_targets
        .iter()
        .flat_map(|target| {
            invalid_escapes(&original_content[target.start..target.end], Context::Link)
                .map(|index| (target.start + index, Context::Link))
        })
        .chain(
            invalid_escapes(original_content, Context::Text)
                .filter(|&index| {
                    !code_spans.contains(source_range.start + index)
                        && !link_targets
                            .iter()
                            .any(|target| target.start <= index && index < target.end)
                })
                .map(|index| (index, Context::Text)),
        )
        .collect::<Vec<_>>();
    invalid_escape_offsets.sort_unstable_by_key(|&(index, _)| index);

    // Reject each such backslash, so it can't be mistaken for an escape, and offer to escape it.
    // The message lists the characters which can be escaped there [ref:content_escapes].
    for (index, context) in invalid_escape_offsets {
        let offset = source_range.start + index;
        errors.push(Error::new(
            invalid_escape_message(context),
            source_path,
            Some((
                source_contents,
                line_index,
                SourceRange {
                    start: offset,
                    end: offset + '\\'.len_utf8(),
                },
            )),
            None,
            Some(Fix::EscapeBackslash(offset)),
        ));
    }

    // Return the links which parsed, along with the errors.
    (links, errors)
}

// Convert the unescaped contents of a closed delimiter pair into a typed link occurrence.
fn parse_link(
    target: &str,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    source_range: SourceRange,
) -> Result<Link, Error> {
    // Report a problem with the link at the link.
    let link_error = |message: &str| {
        Error::new(
            message,
            source_path,
            Some((source_contents, line_index, source_range)),
            None,
            None,
        )
    };

    // Parse a filesystem link's target, which starts with `/`, and take any other target as the
    // title of the page it links to, which an empty link lacks.
    if target.starts_with(FILESYSTEM_LINK_PREFIX) {
        Ok(Link::Filesystem {
            target: FilesystemTarget::parse(target).map_err(|message| link_error(&message))?,
            source_range,
        })
    } else if target.is_empty() {
        Err(link_error("This link is missing a target."))
    } else {
        Ok(Link::Text {
            title: PageTitle::parse(target).map_err(|message| link_error(&message))?,
            source_range,
        })
    }
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
        error::{Error, Fix},
        escaping::Context,
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
                Link::Text { title, .. } => format!("text:{}", title.text(Context::Verbatim)),
                Link::Filesystem { target, .. } => format!(
                    "{}:{}",
                    if target.is_directory() { "dir" } else { "file" },
                    target.path().display(),
                ),
            })
            .collect()
    }

    // Parse titles and multiline content while retaining exact source ranges. A title's range
    // extends through trailing whitespace, which the title itself excludes. Rendered content keeps
    // the indentation of its first line.
    #[test]
    fn pages() {
        let source =
            "  \n#   Home  \n\n Check out the [Greeting]. \n\n# Greeting\n Hello,\nworld! \n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(wiki.page("Home").title.text(Context::Verbatim), "Home");
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Greeting"],
        );
        assert_eq!(
            wiki.to_string(),
            "# Home\n\n Check out the [Greeting].\n\n# Greeting\n\n Hello,\nworld!\n",
        );
        assert_eq!(wiki.page("Home").title_source_range.start, 7);
        assert_eq!(wiki.page("Home").title_source_range.end, 13);
        assert_eq!(wiki.page("Home").source_range.start, 3);
        assert_eq!(wiki.page("Home").source_range.end, 41);
        let Link::Text { source_range, .. } = &wiki.page("Home").links[0] else {
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
            link_targets(&wiki.page("Home").links),
            vec!["text:Greeting", "text:About", "text:Greeting"],
        );
        assert_eq!(
            wiki.to_string(),
            "# Home\n\nSee [Greeting], [About], and [Greeting].\n\n# About\n\n# Greeting\n",
        );
    }

    // Keep source ranges on UTF-8 boundaries when titles and links contain multibyte characters.
    #[test]
    fn unicode_source_ranges() {
        let source = "# Home\nSee [Grüße].\n# Grüße";
        let wiki = parse_test(source).unwrap();
        let Link::Text { source_range, .. } = &wiki.page("Home").links[0] else {
            panic!("The parsed link should be a text link.");
        };

        assert_eq!(&source[source_range.start..source_range.end], "[Grüße]");
        assert_eq!(
            &source[wiki.page("Grüße").title_source_range.start
                ..wiki.page("Grüße").title_source_range.end],
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
            link_targets(&wiki.page("Home").links),
            vec!["text:One]Two", "text:[Three", "text:Four"],
        );
    }

    // Treat an escaped backslash as a literal character which doesn't escape what follows. A title
    // is taken as written, so its backslashes need no escaping.
    #[test]
    fn escaped_backslashes() {
        let wiki = parse_test(
            r"# Home
See \\[Four], [Five\\], [A\\B], and \\\[ignored\].
# Four
# Five\
# A\B",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Four", "text:Five\\", "text:A\\B"],
        );
    }

    // Reject each backslash which doesn't escape anything, including one at the end of a line or a
    // page, offering to escape it, while keeping the page's links.
    #[test]
    fn invalid_escapes() {
        let (wiki, errors) = parse_fixture("# Home\nC:\\Users \\\\ [Other] \\a\\\nend\\\n# Other");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(9)),
                Some(&Fix::EscapeBackslash(27)),
                Some(&Fix::EscapeBackslash(29)),
                Some(&Fix::EscapeBackslash(34)),
            ],
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .contains("A backslash must be followed by")
        }));
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
    }

    // Leave out a link with a backslash which doesn't escape anything, whose target is unreliable,
    // reporting only the backslash.
    #[test]
    fn links_with_invalid_escapes() {
        let (wiki, errors) = parse_fixture("# Home\n[A\\B] [/c\\d.txt] [Other]\n# Other");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(9)),
                Some(&Fix::EscapeBackslash(16)),
            ],
        );
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
    }

    // Reject a backslash before `#`, which can't be escaped, since only a title marker at the start
    // of a line is special.
    #[test]
    fn hashes_cant_be_escaped() {
        let (_wiki, errors) = parse_fixture("# Home\nC\\# and \\#1");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(8)),
                Some(&Fix::EscapeBackslash(15)),
            ],
        );
    }

    // Keep an indented line which starts with a title marker as content, even as the first line of
    // a page's content, so rendering the wiki doesn't turn it into a title.
    #[test]
    fn indented_title_markers() {
        let source = "# Home\n\n  # Not a title\n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 1);
        assert_eq!(wiki.to_string(), source);
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
            link_targets(&wiki.page("Home").links),
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
    // with a trailing `/` and the file root as `/`.
    #[test]
    fn formatted_links() {
        let wiki = parse_test(
            "# Home\n[ Home ] [ /a//b/./c\\[1\\].txt ] [/images/] [/images/./raw//] [/] [/./] \
                [///]",
        )
        .unwrap();

        assert_eq!(
            wiki.to_string(),
            "# Home\n\n[Home] [/a/b/c\\[1\\].txt] [/images/] [/images/raw/] [/] [/] [/]\n",
        );
    }

    // Remove whitespace at the end of each line, including after a link and on blank lines.
    #[test]
    fn trailing_whitespace() {
        let wiki =
            parse_test("# Home\r\nFirst  \t\r\n   \nSee [Home]  \nand [Home] later.\t\nLast")
                .unwrap();

        assert_eq!(
            wiki.to_string(),
            "# Home\n\nFirst\n\nSee [Home]\nand [Home] later.\nLast\n",
        );
    }

    // Reject an empty text link, since no page title can be empty, leaving it out of the page's
    // links.
    #[test]
    fn empty_text_links() {
        let (wiki, errors) = parse_fixture("# Home\nSee [] and [  ] and [Home].");

        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("This link is missing a target.")),
        );
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
    }

    // Take the text of code as written, without titles, links, or escapes, whether it's inline or
    // spans lines, while parsing the text around it as usual. Formatting keeps the code's text.
    #[test]
    fn code_spans() {
        let source = concat!(
            "# Home\n\n",
            "Run `[a] C:\\Users`, not ``x ` y``.\n\n",
            "```sh\n# A comment\n[Not a link] C:\\Users \\[ ]\n```\n\n",
            "See [Other].\n\n",
            "# Other\n",
        );
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
        assert_eq!(wiki.to_string(), source);
    }

    // Treat backticks as plain characters in a title and in a link's target, where formatting
    // leaves them as written, and accept an escaped backtick outside links.
    #[test]
    fn plain_backticks() {
        let source = "# The `ls` command\n\n[The `ls` command] and \\`.\n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(
            link_targets(&wiki.page("The `ls` command").links),
            vec!["text:The `ls` command"],
        );
        assert_eq!(wiki.to_string(), source);
    }

    // Reject an escaped backtick in a link's target, where backticks are plain characters, with a
    // message listing what can be escaped there, and leave out the link. Outside the link, the same
    // escape is accepted.
    #[test]
    fn escaped_backticks_in_links() {
        let (wiki, errors) = parse_fixture("# Home\n\\` [a\\`b] \\` [Home]");

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("In a link, a backslash must be followed by"),
        );
        assert_eq!(errors[0].fix(), Some(&Fix::EscapeBackslash(12)));
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
    }

    // Report a run of backticks which isn't closed, including a short run whose closing run is on
    // a later line, and parse the text after it as usual, so it can't hide the rest of the wiki.
    #[test]
    fn unclosed_code() {
        let (wiki, errors) = parse_fixture("# Home\n```\n[Home] `a\nb`\n# Other");

        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("Unclosed code.")),
        );
        assert!(errors[0].to_string().contains("2 \u{2502} ```"));
        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
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

    // End an unclosed link at the end of its line, reading the following lines as usual, so a later
    // closing delimiter is unexpected and the links after it are kept.
    #[test]
    fn unclosed_links_end_at_line_breaks() {
        let (wiki, errors) =
            parse_fixture("# Home\nSee [Greeting\ncontinued]. [Home] and [Other]\n# Other");

        assert_eq!(errors.len(), 2);
        assert!(errors[0].to_string().contains("Unclosed link."));
        assert!(errors[0].to_string().contains("2 \u{2502} See [Greeting"));
        assert!(
            errors[1]
                .to_string()
                .contains("Unexpected closing link delimiter."),
        );
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Home", "text:Other"],
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

        assert_eq!(wiki.to_string(), "# Home\n\n## Subtitle\n#not a title\n");
    }

    // Accept empty and whitespace-only wikis.
    #[test]
    fn empty_wiki() {
        assert!(parse_test(" \n\t\n").unwrap().pages.is_empty());
    }

    // Accept empty page content.
    #[test]
    fn empty_content() {
        assert_eq!(parse_test("# Empty").unwrap().to_string(), "# Empty\n");
    }

    // Parse Windows line endings without retaining carriage returns.
    #[test]
    fn windows_line_endings() {
        let wiki = parse_test("# Greeting\r\n\r\nHello, world!\r\n").unwrap();

        assert_eq!(wiki.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Reject non-whitespace content before the first title.
    #[test]
    fn content_before_title() {
        assert_fails!(
            parse_test("Introduction\n# Home"),
            "This content isn't in any page.",
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
                .contains("A page title can't be empty."),
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
                    .contains("A page title can't start with `/`."),
            );
        }
    }

    // Don't reinterpret content after an invalid title as content before the first title.
    #[test]
    fn content_after_empty_title() {
        let errors = parse_test("# Home\n\nfoo\n\n# \n\nbar").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A page title can't be empty."),
        );
    }

    // Treat a bare title marker as content, even once formatting removes whitespace after it, so
    // formatting can't turn it into a title.
    #[test]
    fn bare_title_markers() {
        let wiki = parse_test("# Home\n\n#\n#\t\n# \tx").unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(wiki.page("Home").links.len(), 0);
        let formatted = wiki.to_string();
        assert!(formatted.starts_with("# Home\n\n#\n#\n"));
        assert_eq!(parse_test(&formatted).unwrap().pages.len(), 2);
    }

    // Report only the first occurrence of content before a valid title.
    #[test]
    fn repeated_content_before_title() {
        let errors = parse_test("First\nSecond\n# Home").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any page."),
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
                .contains("Page `Home` already exists."),
        );
        assert!(errors[0].to_string().contains("3 \u{2502} # Home"));
    }

    // Report errors from every invalid page in source order.
    #[test]
    fn multiple_page_errors() {
        let errors = parse_test("# First\nUnexpected].\n# Second\nUnclosed [link.").unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[1].to_string().contains("Unclosed link"));
    }

    // Report every delimiter error within one page.
    #[test]
    fn multiple_errors_in_page() {
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

    // Report structural and page errors together in source order.
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
                .contains("This content isn't in any page"),
        );
        assert!(
            errors[1]
                .to_string()
                .contains("A page title can't be empty"),
        );
        assert!(
            errors[2]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[3].to_string().contains("Unclosed link"));
    }

    // Keep a page with a syntax error, along with the links in it which parsed successfully.
    #[test]
    fn recovered_page_keeps_valid_links() {
        let (wiki, errors) = parse_fixture(
            "# Home\nStray] [Greeting] [Bad\nlink] [nested[link] [/../up] [/notes.txt] [unclosed",
        );

        assert_eq!(errors.len(), 6);
        assert_eq!(
            link_targets(&wiki.page("Home").links),
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
        assert_eq!(wiki.page("Greeting").traversal_index, Some(1));
    }

    // Omit duplicate pages and regions without valid titles, but report the errors in their
    // content.
    #[test]
    fn recovered_wiki_omits_untitled_regions() {
        let (wiki, errors) = parse_fixture(
            "Before]\n# Home\n[Home]\n# Home\n[Greeting] a]\n# \n[Greeting] b]\n# Greeting",
        );

        assert_eq!(errors.len(), 6);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any page."),
        );
        assert!(errors[1].to_string().contains("1 \u{2502} Before]"));
        assert!(
            errors[2]
                .to_string()
                .contains("Page `Home` already exists."),
        );
        assert!(errors[3].to_string().contains("5 \u{2502} [Greeting] a]"));
        assert!(
            errors[4]
                .to_string()
                .contains("A page title can't be empty."),
        );
        assert!(errors[5].to_string().contains("7 \u{2502} [Greeting] b]"));
        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
        assert_eq!(wiki.page("Greeting").traversal_index, None);
    }
}
