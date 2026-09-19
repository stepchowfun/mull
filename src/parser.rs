use crate::{
    Errors,
    document::{DIRECTORY_LINK_PREFIX, Document, FILE_LINK_PREFIX, Link, TITLE_PREFIX, TextNode},
    format::CodeStr,
    scoring::populate_depths,
};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

// Parse a filesystem link path while keeping it inside the document's logical tree.
fn parse_filesystem_path(path: &str, node_title: &str) -> Result<PathBuf, String> {
    // Require the document directory or an entry below it without parent or absolute components.
    let parsed_path = Path::new(path);
    let has_invalid_component = parsed_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_),
        )
    });
    let is_valid = !parsed_path.as_os_str().is_empty() && !has_invalid_component;
    if !is_valid {
        return Err(format!(
            concat!(
                "Filesystem link path {} in node {} must identify the document directory or an ",
                "entry below it without using {}.",
            ),
            parsed_path.to_string_lossy().code_str(),
            node_title.code_str(),
            "..".code_str(),
        ));
    }

    // Normalize harmless current-directory components without resolving symlinks.
    Ok(parsed_path
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                unreachable!("filesystem link path components were already validated")
            }
        })
        .collect())
}

// Add a completed node after collecting all of its parsing errors.
fn insert_node(
    document: &mut Document,
    title: String,
    title_line: usize,
    content_lines: &[&str],
) -> Result<(), Errors> {
    // Strip whitespace around the content while preserving its internal formatting.
    let original_content = content_lines.join("\n").trim().to_owned();

    // Collect link targets and rebuild the content with their surrounding whitespace stripped.
    let mut content = String::new();
    let mut copied_through = 0;
    let mut links = HashSet::<Link>::new();
    let mut errors = Vec::<String>::new();
    let mut link_start = None::<usize>;
    let mut link_has_line_break = false;
    let mut previous_was_backslash = false;
    for (index, character) in original_content.char_indices() {
        let is_escaped_delimiter = previous_was_backslash && matches!(character, '[' | ']');
        previous_was_backslash = character == '\\';
        if is_escaped_delimiter {
            continue;
        }

        // Links must fit on a single line.
        if character == '\n' && link_start.is_some() && !link_has_line_break {
            errors.push(format!(
                "Link in node {} contains a line break.",
                title.code_str(),
            ));
            link_has_line_break = true;
        }

        match character {
            '[' => {
                if link_start.is_some() {
                    errors.push(format!(
                        "Unexpected opening link delimiter in node {}.",
                        title.code_str(),
                    ));
                } else {
                    link_start = Some(index + character.len_utf8());
                    link_has_line_break = false;
                }
            }
            ']' => {
                if let Some(start) = link_start.take() {
                    let original_link = &original_content[start..index];
                    let trimmed_link = original_link.trim();
                    let link = trimmed_link.replace("\\[", "[").replace("\\]", "]");
                    if let Some(path) = link.strip_prefix(FILE_LINK_PREFIX) {
                        match parse_filesystem_path(path, &title) {
                            Ok(path) => {
                                links.insert(Link::File(path));
                            }
                            Err(error) => errors.push(error),
                        }
                    } else if let Some(path) = link.strip_prefix(DIRECTORY_LINK_PREFIX) {
                        match parse_filesystem_path(path, &title) {
                            Ok(path) => {
                                links.insert(Link::Directory(path));
                            }
                            Err(error) => errors.push(error),
                        }
                    } else {
                        links.insert(Link::Text(link));
                    }
                    content.push_str(&original_content[copied_through..start]);
                    content.push_str(trimmed_link);
                    content.push(']');
                    copied_through = index + character.len_utf8();
                } else {
                    errors.push(format!(
                        "Unexpected closing link delimiter in node {}.",
                        title.code_str(),
                    ));
                }
            }
            _ => {}
        }
    }

    // Reject an opening delimiter that has no closing delimiter.
    if link_start.is_some() {
        errors.push(format!("Unclosed link in node {}.", title.code_str()));
    }

    // Retain the content following the final link.
    content.push_str(&original_content[copied_through..]);

    // Reject a title that has already been used.
    if document.text_nodes.contains_key(&title) {
        errors.push(format!(
            "Duplicate title {} on line {title_line}.",
            title.code_str(),
        ));
    }

    // Insert only nodes that parsed without errors.
    if errors.is_empty() {
        document.text_nodes.insert(
            title.clone(),
            TextNode {
                title,
                content,
                links,
                depth: None,
            },
        );
        Ok(())
    } else {
        Err(errors)
    }
}

// Parse source contents into a scored document.
pub fn parse(contents: &str) -> Result<Document, Errors> {
    // Accumulate the parsed document, node errors, and the node currently being read.
    let mut document = Document::default();
    let mut errors = Vec::<String>::new();
    let mut current_title = None::<(String, usize)>;
    let mut content_lines = Vec::<&str>::new();
    let mut reported_content_before_title = false;

    // Process title lines as boundaries and retain all other lines as content.
    for (line_index, line) in contents.lines().enumerate() {
        let line_number = line_index + 1;
        if let Some(title) = line.strip_prefix(TITLE_PREFIX) {
            // Finish the preceding node before starting the next one.
            if let Some((title, title_line)) = current_title.take() {
                if let Err(node_errors) =
                    insert_node(&mut document, title, title_line, &content_lines)
                {
                    errors.extend(node_errors);
                }
                content_lines.clear();
            }

            // Reject titles that are empty after surrounding whitespace is stripped.
            let title = title.trim();
            if title.is_empty() {
                errors.push(format!("Title on line {line_number} is empty."));
            } else {
                current_title = Some((title.to_owned(), line_number));
            }
        } else if current_title.is_some() {
            // Preserve lines belonging to the current node until its content is finalized.
            content_lines.push(line);
        } else if !reported_content_before_title && !line.trim().is_empty() {
            // Report only the first non-whitespace content outside a valid node.
            errors.push(format!(
                "Content appears before the first title on line {line_number}.",
            ));
            reported_content_before_title = true;
        }
    }

    // Finish the final node at the end of the document.
    if let Some((title, title_line)) = current_title
        && let Err(node_errors) = insert_node(&mut document, title, title_line, &content_lines)
    {
        errors.extend(node_errors);
    }

    // Return all node errors together, or score and return the parsed document.
    if errors.is_empty() {
        // Populate minimum distances from the home node in the parsed document.
        populate_depths(&mut document);
        Ok(document)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::document::Link;
    use std::collections::HashSet;
    use std::path::PathBuf;

    // Parse titles and multiline content while stripping surrounding whitespace.
    #[test]
    fn nodes() {
        let document = parse(
            "  \n#   Home  \n\n Check out the [Greeting]. \n\n# Greeting\n Hello,\nworld! \n",
        )
        .unwrap();

        assert_eq!(document.text_nodes.len(), 2);
        assert_eq!(document.text_nodes["Home"].title, "Home");
        assert_eq!(
            document.text_nodes["Home"].content,
            "Check out the [Greeting].",
        );
        assert_eq!(
            document.text_nodes["Home"].links,
            HashSet::from([Link::Text("Greeting".to_owned())]),
        );
        assert_eq!(document.text_nodes["Greeting"].content, "Hello,\nworld!");
    }

    // Parse distinct text links while stripping their surrounding whitespace.
    #[test]
    fn links() {
        let document = parse(concat!(
            "# Home\nSee [Greeting], [ About ], and [Greeting].",
            "\n# About\n# Greeting",
        ))
        .unwrap();

        assert_eq!(
            document.text_nodes["Home"].links,
            HashSet::from([
                Link::Text("About".to_owned()),
                Link::Text("Greeting".to_owned()),
            ]),
        );
        assert_eq!(
            document.text_nodes["Home"].content,
            "See [Greeting], [About], and [Greeting].",
        );
    }

    // Treat escaped square brackets as literal link-title characters.
    #[test]
    fn escaped_link_delimiters() {
        let document = parse(
            r"# Home
See \[Ignored\], [One\]Two], [\[Three], [Four], and \[also ignored\].
# Four
# One]Two
# [Three",
        )
        .unwrap();

        assert_eq!(
            document.text_nodes["Home"].links,
            HashSet::from([
                Link::Text("Four".to_owned()),
                Link::Text("One]Two".to_owned()),
                Link::Text("[Three".to_owned()),
            ]),
        );
    }

    // Parse file and directory links separately from text links.
    #[test]
    fn filesystem_links() {
        let document = parse(concat!(
            "# Home\nSee [",
            "file:./notes.txt], [",
            "dir:images], and [",
            "dir:images/./raw], plus [",
            "dir:.].",
        ))
        .unwrap();

        assert_eq!(
            document.text_nodes["Home"].links,
            HashSet::from([
                Link::File(PathBuf::from("notes.txt")),
                Link::Directory(PathBuf::new()),
                Link::Directory(PathBuf::from("images")),
                Link::Directory(PathBuf::from("images/raw")),
            ]),
        );
    }

    // Reject filesystem links that are empty, absolute, or contain parents.
    #[test]
    fn invalid_filesystem_link_paths() {
        assert_eq!(
            parse(concat!(
                "# Home\n[",
                "file:] [",
                "file:../notes.txt] [",
                "dir:/images]",
            ))
            .unwrap_err(),
            vec![
                concat!(
                    "Filesystem link path `` in node `Home` must identify the document directory ",
                    "or an entry below it without using `..`.",
                )
                .to_owned(),
                concat!(
                    "Filesystem link path `../notes.txt` in node `Home` must identify the ",
                    "document directory or an entry below it without using `..`.",
                )
                .to_owned(),
                concat!(
                    "Filesystem link path `/images` in node `Home` must identify the document ",
                    "directory or an entry below it without using `..`.",
                )
                .to_owned(),
            ],
        );
    }

    // Reject opening link delimiters that are not closed.
    #[test]
    fn unclosed_link() {
        assert_eq!(
            parse("# Home\nSee [Greeting.").unwrap_err(),
            vec!["Unclosed link in node `Home`.".to_owned()],
        );
    }

    // Reject links that span multiple lines.
    #[test]
    fn link_with_line_break() {
        assert_eq!(
            parse("# Home\nSee [Greeting\ncontinued].").unwrap_err(),
            vec!["Link in node `Home` contains a line break.".to_owned()],
        );
    }

    // Reject unescaped opening delimiters inside links.
    #[test]
    fn unexpected_opening_delimiter() {
        assert_eq!(
            parse("# Home\nSee [nested[Greeting].").unwrap_err(),
            vec!["Unexpected opening link delimiter in node `Home`.".to_owned()],
        );
    }

    // Reject unescaped closing delimiters outside links.
    #[test]
    fn unexpected_closing_delimiter() {
        assert_eq!(
            parse("# Home\nSee Greeting].").unwrap_err(),
            vec!["Unexpected closing link delimiter in node `Home`.".to_owned()],
        );
    }

    // Treat hashes without the required trailing space as ordinary content.
    #[test]
    fn non_title_hashes() {
        let document = parse("# Home\n\n## Subtitle\n#not a title").unwrap();

        assert_eq!(
            document.text_nodes["Home"].content,
            "## Subtitle\n#not a title",
        );
    }

    // Accept empty and whitespace-only documents.
    #[test]
    fn empty_document() {
        assert!(parse(" \n\t\n").unwrap().text_nodes.is_empty());
    }

    // Accept empty node content.
    #[test]
    fn empty_content() {
        assert!(
            parse("# Empty").unwrap().text_nodes["Empty"]
                .content
                .is_empty(),
        );
    }

    // Parse Windows line endings without retaining carriage returns.
    #[test]
    fn windows_line_endings() {
        let document = parse("# Greeting\r\n\r\nHello, world!\r\n").unwrap();

        assert_eq!(document.text_nodes["Greeting"].content, "Hello, world!");
    }

    // Reject non-whitespace content before the first title.
    #[test]
    fn content_before_title() {
        assert_eq!(
            parse("Introduction\n# Home").unwrap_err(),
            vec!["Content appears before the first title on line 1.".to_owned()],
        );
    }

    // Reject titles that are empty after whitespace is stripped.
    #[test]
    fn empty_title() {
        assert_eq!(
            parse("#   \nContent").unwrap_err(),
            vec![
                "Title on line 1 is empty.".to_owned(),
                "Content appears before the first title on line 2.".to_owned(),
            ],
        );
    }

    // Report only the first occurrence of content before a valid title.
    #[test]
    fn repeated_content_before_title() {
        assert_eq!(
            parse("First\nSecond\n# Home").unwrap_err(),
            vec!["Content appears before the first title on line 1.".to_owned()],
        );
    }

    // Reject duplicate titles rather than silently replacing a node.
    #[test]
    fn duplicate_title() {
        assert_eq!(
            parse("# Home\nFirst\n# Home\nSecond").unwrap_err(),
            vec!["Duplicate title `Home` on line 3.".to_owned()],
        );
    }

    // Report errors from every invalid node in source order.
    #[test]
    fn multiple_node_errors() {
        assert_eq!(
            parse("# First\nUnexpected].\n# Second\nUnclosed [link.").unwrap_err(),
            vec![
                "Unexpected closing link delimiter in node `First`.".to_owned(),
                "Unclosed link in node `Second`.".to_owned(),
            ],
        );
    }

    // Report every delimiter error within one node.
    #[test]
    fn multiple_errors_in_node() {
        assert_eq!(
            parse("# Home\nUnexpected] and [nested[link.").unwrap_err(),
            vec![
                "Unexpected closing link delimiter in node `Home`.".to_owned(),
                "Unexpected opening link delimiter in node `Home`.".to_owned(),
                "Unclosed link in node `Home`.".to_owned(),
            ],
        );
    }

    // Report structural and node errors together in source order.
    #[test]
    fn multiple_error_types() {
        assert_eq!(
            parse(concat!(
                "Introduction\n",
                "#   \n",
                "Content\n",
                "# First\n",
                "Unexpected].\n",
                "# Second\n",
                "Unclosed [link.",
            ))
            .unwrap_err(),
            vec![
                "Content appears before the first title on line 1.".to_owned(),
                "Title on line 2 is empty.".to_owned(),
                "Unexpected closing link delimiter in node `First`.".to_owned(),
                "Unclosed link in node `Second`.".to_owned(),
            ],
        );
    }
}
