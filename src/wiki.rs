use crate::error::SourceRange;
use std::{collections::HashMap, fmt, path::PathBuf};

// These strings define the wiki format's extension and structural markers.
pub const WIKI_EXTENSION: &str = "mull";
pub const TITLE_PREFIX: &str = "# ";
pub const FILE_LINK_PREFIX: &str = "file:";
pub const DIRECTORY_LINK_PREFIX: &str = "dir:";

// This title identifies the root of every wiki's text-link graph.
pub const HOME_TITLE: &str = "Home";

// These are the source occurrences through which a node can reference a target.
#[derive(Clone, Debug)]
pub enum Link {
    Text {
        title: String,
        source_range: SourceRange,
    },
    File {
        path: PathBuf,
        source_range: SourceRange,
    },
    Directory {
        path: PathBuf,
        source_range: SourceRange,
    },
}

// This struct represents a text node in a wiki.
#[derive(Clone, Debug)]
pub struct TextNode {
    pub title: String, // Non-empty, no line breaks, and no leading or trailing whitespace
    pub content: String, // No leading or trailing whitespace
    pub links: Vec<Link>,
    pub depth: Option<usize>, // Minimum text-link distance from the root
    #[allow(
        dead_code,
        reason = "Retained for diagnostics covering a complete text node."
    )]
    pub source_range: SourceRange, // The complete node
    pub title_source_range: SourceRange, // The trimmed title text
}

// This struct represents a parsed wiki.
#[derive(Clone, Debug, Default)]
pub struct Wiki {
    pub text_nodes: HashMap<String, TextNode>,
}

// Render nodes in the wiki's heading-and-content format.
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
impl fmt::Display for Wiki {
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
    use super::{TextNode, Wiki};
    use crate::error::SourceRange;
    use std::collections::HashMap;

    // Use a harmless range when testing rendering, which does not inspect source locations.
    const SOURCE_RANGE: SourceRange = SourceRange { start: 0, end: 0 };

    // Ensure nodes are rendered in the wiki's source format.
    #[test]
    fn node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
            links: Vec::new(),
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Ensure empty nodes do not contain a redundant content separator.
    #[test]
    fn empty_node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: String::new(),
            links: Vec::new(),
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(node.to_string(), "# Greeting\n");
    }

    // Ensure a wiki containing an empty node has only its trailing line break.
    #[test]
    fn empty_node_wiki_display() {
        let wiki = Wiki {
            text_nodes: HashMap::from([(
                "Greeting".to_owned(),
                TextNode {
                    title: "Greeting".to_owned(),
                    content: String::new(),
                    links: Vec::new(),
                    depth: None,
                    source_range: SOURCE_RANGE,
                    title_source_range: SOURCE_RANGE,
                },
            )]),
        };

        assert_eq!(wiki.to_string(), "# Greeting\n");
    }

    // Render nodes by depth and title, placing nodes without a depth last.
    #[test]
    fn wiki_display() {
        let wiki = Wiki {
            text_nodes: HashMap::from([
                (
                    "Greeting".to_owned(),
                    TextNode {
                        title: "Greeting".to_owned(),
                        content: "Hello, world!".to_owned(),
                        links: Vec::new(),
                        depth: Some(1),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                    },
                ),
                (
                    "Home".to_owned(),
                    TextNode {
                        title: "Home".to_owned(),
                        content: "Check out the [Greeting].".to_owned(),
                        links: Vec::new(),
                        depth: Some(0),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                    },
                ),
                (
                    "Orphan".to_owned(),
                    TextNode {
                        title: "Orphan".to_owned(),
                        content: String::new(),
                        links: Vec::new(),
                        depth: None,
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                    },
                ),
            ]),
        };

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\nCheck out the [Greeting].\n\n",
                "# Greeting\n\nHello, world!\n\n",
                "# Orphan\n",
            ),
        );
    }

    // Ensure an empty wiki has no output.
    #[test]
    fn empty_wiki_display() {
        assert_eq!(Wiki::default().to_string(), "");
    }
}
