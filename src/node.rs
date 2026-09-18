use std::fmt;

// This struct represents a node in a Mull document.
#[derive(Clone, Debug)]
pub struct Node {
    pub title: String,
    pub content: String,
}

// Render nodes in Mull's heading-and-content format.
impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "# {}\n\n{}", self.title, self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::Node;

    // Ensure nodes are rendered in Mull's source format.
    #[test]
    fn display() {
        let node = Node {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!");
    }
}
