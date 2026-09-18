use crate::node::Node;
use std::collections::HashMap;

// Parse a Mull document into nodes indexed by title.
#[must_use]
pub fn parse(_document: &str) -> HashMap<String, Node> {
    // Parsing will be implemented later.
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::parse;

    // The parser returns no nodes until parsing is implemented.
    #[test]
    fn empty_result() {
        assert!(parse("# Greeting\n\nHello, world!").is_empty());
    }
}
