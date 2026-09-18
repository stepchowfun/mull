use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::PathBuf,
};

// These strings define the document format's extension and structural markers.
pub const DOCUMENT_EXTENSION: &str = "mull";
pub const TITLE_PREFIX: &str = "# ";
pub const FILE_LINK_PREFIX: &str = "file:";
pub const DIRECTORY_LINK_PREFIX: &str = "dir:";

// This title identifies the root of every document's text-link graph.
pub const HOME_TITLE: &str = "Home";

// These are the targets that a node can reference.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Link {
    Text(String),
    File(PathBuf),
    Directory(PathBuf),
}

// This struct represents a text node in a document.
#[derive(Clone, Debug)]
pub struct TextNode {
    pub title: String, // Non-empty, no line breaks, and no leading or trailing whitespace
    pub content: String, // No leading or trailing whitespace
    pub links: HashSet<Link>,
    pub depth: Option<usize>, // Minimum text-link distance from the root, populated by validation
}

// This struct represents a parsed document.
#[derive(Clone, Debug, Default)]
pub struct Document {
    pub text_nodes: HashMap<String, TextNode>,
}

// Render nodes in the document's heading-and-content format.
impl fmt::Display for TextNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Omit the content separator when there is no content.
        if self.content.is_empty() {
            writeln!(formatter, "{TITLE_PREFIX}{}", self.title)
        } else {
            writeln!(
                formatter,
                "{TITLE_PREFIX}{}\n\n{}",
                self.title,
                self.content,
            )
        }
    }
}

// Render nodes deterministically in depth order with titles breaking ties.
impl fmt::Display for Document {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort reachable nodes by depth and title, followed by unreachable nodes in title order.
        let mut nodes = self.text_nodes.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|(title, node)| (node.depth.is_none(), node.depth, *title));

        // Add one line break between nodes because each node already ends with one.
        for (index, (_title, node)) in nodes.into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            write!(formatter, "{node}")?;
        }

        // Rendering succeeded.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Document, TextNode};
    use std::collections::{HashMap, HashSet};

    // Ensure nodes are rendered in the document's source format.
    #[test]
    fn node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
            links: HashSet::new(),
            depth: None,
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Ensure empty nodes do not contain a redundant content separator.
    #[test]
    fn empty_node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: String::new(),
            links: HashSet::new(),
            depth: None,
        };

        assert_eq!(node.to_string(), "# Greeting\n");
    }

    // Ensure a document containing an empty node has only its trailing line break.
    #[test]
    fn empty_node_document_display() {
        let document = Document {
            text_nodes: HashMap::from([(
                "Greeting".to_owned(),
                TextNode {
                    title: "Greeting".to_owned(),
                    content: String::new(),
                    links: HashSet::new(),
                    depth: None,
                },
            )]),
        };

        assert_eq!(document.to_string(), "# Greeting\n");
    }

    // Render nodes by depth and title, placing nodes without a depth last.
    #[test]
    fn document_display() {
        let document = Document {
            text_nodes: HashMap::from([
                (
                    "Greeting".to_owned(),
                    TextNode {
                        title: "Greeting".to_owned(),
                        content: "Hello, world!".to_owned(),
                        links: HashSet::new(),
                        depth: Some(1),
                    },
                ),
                (
                    "Home".to_owned(),
                    TextNode {
                        title: "Home".to_owned(),
                        content: "Check out the [Greeting].".to_owned(),
                        links: HashSet::new(),
                        depth: Some(0),
                    },
                ),
                (
                    "Orphan".to_owned(),
                    TextNode {
                        title: "Orphan".to_owned(),
                        content: String::new(),
                        links: HashSet::new(),
                        depth: None,
                    },
                ),
            ]),
        };

        assert_eq!(
            document.to_string(),
            concat!(
                "# Home\n\nCheck out the [Greeting].\n\n",
                "# Greeting\n\nHello, world!\n\n",
                "# Orphan\n",
            ),
        );
    }

    // Ensure an empty document has no output.
    #[test]
    fn empty_document_display() {
        assert_eq!(Document::default().to_string(), "");
    }
}
