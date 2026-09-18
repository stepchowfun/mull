use std::{
    collections::{HashMap, HashSet},
    fmt,
};

// This struct represents a node in a document.
#[derive(Clone, Debug)]
pub struct Node {
    pub title: String, // Non-empty, no line breaks, and no leading or trailing whitespace
    pub content: String, // No leading or trailing whitespace
    pub links: HashSet<String>, // Node titles
}

// This struct represents a parsed document.
#[derive(Clone, Debug, Default)]
pub struct Document {
    pub nodes: HashMap<String, Node>,
}

// Render nodes in the document's heading-and-content format.
impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Omit the content separator when there is no content.
        if self.content.is_empty() {
            writeln!(formatter, "# {}", self.title)
        } else {
            writeln!(formatter, "# {}\n\n{}", self.title, self.content)
        }
    }
}

// Render nodes deterministically in title order.
impl fmt::Display for Document {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort the nodes by their map keys so rendering does not depend on hash iteration order.
        let mut nodes = self.nodes.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|(title, _node)| *title);

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
    use super::{Document, Node};
    use std::collections::{HashMap, HashSet};

    // Ensure nodes are rendered in the document's source format.
    #[test]
    fn node_display() {
        let node = Node {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
            links: HashSet::new(),
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Ensure empty nodes do not contain a redundant content separator.
    #[test]
    fn empty_node_display() {
        let node = Node {
            title: "Greeting".to_owned(),
            content: String::new(),
            links: HashSet::new(),
        };

        assert_eq!(node.to_string(), "# Greeting\n");
    }

    // Ensure a document containing an empty node has only its trailing line break.
    #[test]
    fn empty_node_document_display() {
        let document = Document {
            nodes: HashMap::from([(
                "Greeting".to_owned(),
                Node {
                    title: "Greeting".to_owned(),
                    content: String::new(),
                    links: HashSet::new(),
                },
            )]),
        };

        assert_eq!(document.to_string(), "# Greeting\n");
    }

    // Ensure documents are rendered deterministically with blank lines between nodes.
    #[test]
    fn document_display() {
        let document = Document {
            nodes: HashMap::from([
                (
                    "Greeting".to_owned(),
                    Node {
                        title: "Greeting".to_owned(),
                        content: "Hello, world!".to_owned(),
                        links: HashSet::new(),
                    },
                ),
                (
                    "Home".to_owned(),
                    Node {
                        title: "Home".to_owned(),
                        content: "Check out the [Greeting].".to_owned(),
                        links: HashSet::new(),
                    },
                ),
            ]),
        };

        assert_eq!(
            document.to_string(),
            "# Greeting\n\nHello, world!\n\n# Home\n\nCheck out the [Greeting].\n",
        );
    }

    // Ensure an empty document has no output.
    #[test]
    fn empty_document_display() {
        assert_eq!(Document::default().to_string(), "");
    }
}
