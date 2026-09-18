use std::{collections::HashMap, fmt};

// This struct represents a node in a document.
#[derive(Clone, Debug)]
pub struct Node {
    pub title: String,
    pub content: String,
}

// This struct represents a parsed document.
#[derive(Clone, Debug, Default)]
pub struct Document {
    pub nodes: HashMap<String, Node>,
}

// Render nodes in the document's heading-and-content format.
impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "# {}\n\n{}", self.title, self.content)
    }
}

// Render nodes deterministically in title order.
impl fmt::Display for Document {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort the nodes by their map keys so rendering does not depend on hash iteration order.
        let mut nodes = self.nodes.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|(title, _node)| *title);

        // Separate adjacent nodes with one blank line.
        for (index, (_title, node)) in nodes.into_iter().enumerate() {
            if index > 0 {
                write!(formatter, "\n\n")?;
            }
            write!(formatter, "{node}")?;
        }

        // End a non-empty document with a line break.
        if self.nodes.is_empty() {
            Ok(())
        } else {
            writeln!(formatter)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Document, Node};
    use std::collections::HashMap;

    // Ensure nodes are rendered in the document's source format.
    #[test]
    fn node_display() {
        let node = Node {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!");
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
                    },
                ),
                (
                    "Home".to_owned(),
                    Node {
                        title: "Home".to_owned(),
                        content: "Check out the [Greeting].".to_owned(),
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
