use crate::document::{Document, Node};
use std::collections::hash_map::Entry;

// Add a completed node to the document while rejecting duplicate titles.
fn insert_node(
    document: &mut Document,
    title: String,
    title_line: usize,
    content_lines: &[&str],
) -> Result<(), String> {
    // Strip whitespace around the content while preserving its internal formatting.
    let content = content_lines.join("\n").trim().to_owned();

    // Insert the node unless its title has already been used.
    match document.nodes.entry(title.clone()) {
        Entry::Occupied(_) => Err(format!("Duplicate title {title:?} on line {title_line}.")),
        Entry::Vacant(entry) => {
            entry.insert(Node { title, content });
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

    // Parsing succeeded.
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::parse;

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
        assert_eq!(document.nodes["Greeting"].content, "Hello,\nworld!");
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
