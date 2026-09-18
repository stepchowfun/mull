use crate::document::{Document, Node};
use std::collections::{HashSet, hash_map::Entry};

// Add a completed node to the document while rejecting duplicate titles.
fn insert_node(
    document: &mut Document,
    title: String,
    title_line: usize,
    content_lines: &[&str],
) -> Result<(), String> {
    // Strip whitespace around the content while preserving its internal formatting.
    let original_content = content_lines.join("\n").trim().to_owned();

    // Collect link titles and rebuild the content with their surrounding whitespace stripped.
    let mut content = String::new();
    let mut copied_through = 0;
    let mut links = HashSet::<String>::new();
    let mut link_start = None::<usize>;
    let mut previous_was_backslash = false;
    for (index, character) in original_content.char_indices() {
        let is_escaped_delimiter = previous_was_backslash && matches!(character, '[' | ']');
        previous_was_backslash = character == '\\';
        if is_escaped_delimiter {
            continue;
        }

        // Links must fit on a single line.
        if character == '\n' && link_start.is_some() {
            return Err(format!("Link in node {title:?} contains a line break."));
        }

        match character {
            '[' => {
                if link_start.is_some() {
                    return Err(format!(
                        "Unexpected opening link delimiter in node {title:?}.",
                    ));
                }
                link_start = Some(index + character.len_utf8());
            }
            ']' => {
                let Some(start) = link_start.take() else {
                    return Err(format!(
                        "Unexpected closing link delimiter in node {title:?}.",
                    ));
                };
                let original_link = &original_content[start..index];
                let link = original_link.trim();
                links.insert(link.replace("\\[", "[").replace("\\]", "]"));
                content.push_str(&original_content[copied_through..start]);
                content.push_str(link);
                content.push(']');
                copied_through = index + character.len_utf8();
            }
            _ => {}
        }
    }

    // Reject an opening delimiter that has no closing delimiter.
    if link_start.is_some() {
        return Err(format!("Unclosed link in node {title:?}."));
    }

    // Retain the content following the final link.
    content.push_str(&original_content[copied_through..]);

    // Insert the node unless its title has already been used.
    match document.nodes.entry(title.clone()) {
        Entry::Occupied(_) => Err(format!("Duplicate title {title:?} on line {title_line}.")),
        Entry::Vacant(entry) => {
            entry.insert(Node {
                title,
                content,
                links,
            });
            Ok(())
        }
    }
}

// Parse source contents into a document.
pub fn parse(contents: &str) -> Result<Document, String> {
    // Accumulate the parsed document and the node currently being read.
    let mut document = Document::default();
    let mut current_title = None::<(String, usize)>;
    let mut content_lines = Vec::<&str>::new();

    // Process title lines as boundaries and retain all other lines as content.
    for (line_index, line) in contents.lines().enumerate() {
        let line_number = line_index + 1;
        if let Some(title) = line.strip_prefix("# ") {
            // Finish the preceding node before starting the next one.
            if let Some((title, title_line)) = current_title.take() {
                insert_node(&mut document, title, title_line, &content_lines)?;
                content_lines.clear();
            }

            // Reject titles that are empty after surrounding whitespace is stripped.
            let title = title.trim();
            if title.is_empty() {
                return Err(format!("Title on line {line_number} is empty."));
            }
            current_title = Some((title.to_owned(), line_number));
        } else if current_title.is_some() {
            // Preserve lines belonging to the current node until its content is finalized.
            content_lines.push(line);
        } else if !line.trim().is_empty() {
            // Non-whitespace content cannot appear before the first title.
            return Err(format!(
                "Content appears before the first title on line {line_number}.",
            ));
        }
    }

    // Finish the final node at the end of the document.
    if let Some((title, title_line)) = current_title {
        insert_node(&mut document, title, title_line, &content_lines)?;
    }

    // Validate links deterministically after every node is available.
    let mut nodes = document.nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    for node in nodes {
        let mut links = node.links.iter().collect::<Vec<_>>();
        links.sort();
        for link in links {
            if !document.nodes.contains_key(link) {
                return Err(format!(
                    "Node {:?} links to missing node {link:?}.",
                    node.title,
                ));
            }
        }
    }

    // Parsing succeeded.
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use std::collections::HashSet;

    // Parse titles and multiline content while stripping surrounding whitespace.
    #[test]
    fn nodes() {
        let document = parse(
            "  \n#   Home  \n\n Check out the [Greeting]. \n\n# Greeting\n Hello,\nworld! \n",
        )
        .unwrap();

        assert_eq!(document.nodes.len(), 2);
        assert_eq!(document.nodes["Home"].title, "Home");
        assert_eq!(document.nodes["Home"].content, "Check out the [Greeting].");
        assert_eq!(
            document.nodes["Home"].links,
            HashSet::from(["Greeting".to_owned()]),
        );
        assert_eq!(document.nodes["Greeting"].content, "Hello,\nworld!");
    }

    // Parse distinct links while stripping their surrounding whitespace.
    #[test]
    fn links() {
        let document = parse(concat!(
            "# Home\nSee [Greeting], [ About ], and [Greeting].",
            "\n# About\n# Greeting",
        ))
        .unwrap();

        assert_eq!(
            document.nodes["Home"].links,
            HashSet::from(["About".to_owned(), "Greeting".to_owned()]),
        );
        assert_eq!(
            document.nodes["Home"].content,
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
            document.nodes["Home"].links,
            HashSet::from(["Four".to_owned(), "One]Two".to_owned(), "[Three".to_owned()]),
        );
    }

    // Reject links that do not correspond to any node in the document.
    #[test]
    fn missing_link() {
        assert_eq!(
            parse("# Home\nSee [Zulu] and [Alpha].").unwrap_err(),
            "Node \"Home\" links to missing node \"Alpha\".",
        );
    }

    // Parse an empty link and reject it because node titles cannot be empty.
    #[test]
    fn empty_link() {
        assert_eq!(
            parse("# Home\nSee [].").unwrap_err(),
            "Node \"Home\" links to missing node \"\".",
        );
    }

    // Reject opening link delimiters that are not closed.
    #[test]
    fn unclosed_link() {
        assert_eq!(
            parse("# Home\nSee [Greeting.").unwrap_err(),
            "Unclosed link in node \"Home\".",
        );
    }

    // Reject links that span multiple lines.
    #[test]
    fn link_with_line_break() {
        assert_eq!(
            parse("# Home\nSee [Greeting\ncontinued].").unwrap_err(),
            "Link in node \"Home\" contains a line break.",
        );
    }

    // Reject unescaped opening delimiters inside links.
    #[test]
    fn unexpected_opening_delimiter() {
        assert_eq!(
            parse("# Home\nSee [nested[Greeting].").unwrap_err(),
            "Unexpected opening link delimiter in node \"Home\".",
        );
    }

    // Reject unescaped closing delimiters outside links.
    #[test]
    fn unexpected_closing_delimiter() {
        assert_eq!(
            parse("# Home\nSee Greeting].").unwrap_err(),
            "Unexpected closing link delimiter in node \"Home\".",
        );
    }

    // Treat hashes without the required trailing space as ordinary content.
    #[test]
    fn non_title_hashes() {
        let document = parse("# Home\n\n## Subtitle\n#not a title").unwrap();

        assert_eq!(document.nodes["Home"].content, "## Subtitle\n#not a title");
    }

    // Accept empty and whitespace-only documents.
    #[test]
    fn empty_document() {
        assert!(parse(" \n\t\n").unwrap().nodes.is_empty());
    }

    // Accept empty node content.
    #[test]
    fn empty_content() {
        assert!(parse("# Empty").unwrap().nodes["Empty"].content.is_empty());
    }

    // Parse Windows line endings without retaining carriage returns.
    #[test]
    fn windows_line_endings() {
        let document = parse("# Greeting\r\n\r\nHello, world!\r\n").unwrap();

        assert_eq!(document.nodes["Greeting"].content, "Hello, world!");
    }

    // Reject non-whitespace content before the first title.
    #[test]
    fn content_before_title() {
        assert_eq!(
            parse("Introduction\n# Home").unwrap_err(),
            "Content appears before the first title on line 1.",
        );
    }

    // Reject titles that are empty after whitespace is stripped.
    #[test]
    fn empty_title() {
        assert_eq!(
            parse("#   \nContent").unwrap_err(),
            "Title on line 1 is empty.",
        );
    }

    // Reject duplicate titles rather than silently replacing a node.
    #[test]
    fn duplicate_title() {
        assert_eq!(
            parse("# Home\nFirst\n# Home\nSecond").unwrap_err(),
            "Duplicate title \"Home\" on line 3.",
        );
    }
}
