use crate::wiki::{HOME_TITLE, Link, Wiki};
use std::collections::VecDeque;

// Populate minimum text-link distances from the home node using breadth-first traversal.
pub fn populate_depths(wiki: &mut Wiki) {
    // Reset depths before recalculating them.
    for node in wiki.text_nodes.values_mut() {
        node.depth = None;
    }

    // Start traversal at the home node when it exists.
    let mut pending_titles = VecDeque::<String>::new();
    if let Some(home) = wiki.text_nodes.get_mut(HOME_TITLE) {
        home.depth = Some(0);
        pending_titles.push_back(HOME_TITLE.to_owned());
    }

    // Score each newly reached node and queue it for traversal.
    while let Some(title) = pending_titles.pop_front() {
        let depth = wiki.text_nodes[&title]
            .depth
            .expect("queued nodes should have a depth");
        let text_links = wiki.text_nodes[&title]
            .links
            .iter()
            .filter_map(|link| match link {
                Link::Text(text_link) => Some(text_link.clone()),
                Link::File(_) | Link::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        for text_link in text_links {
            if let Some(target) = wiki.text_nodes.get_mut(&text_link)
                && target.depth.is_none()
            {
                target.depth = Some(depth + 1);
                pending_titles.push_back(text_link);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::populate_depths;
    use crate::parser::parse;

    // Populate depths for nodes reached through multiple levels of text links.
    #[test]
    fn transitive_text_links() {
        let mut wiki = parse("# Home\nSee [Middle].\n# Middle\nSee [End].\n# End").unwrap();

        populate_depths(&mut wiki);

        assert_eq!(wiki.text_nodes["Home"].depth, Some(0));
        assert_eq!(wiki.text_nodes["Middle"].depth, Some(1));
        assert_eq!(wiki.text_nodes["End"].depth, Some(2));
    }

    // Choose the shortest distance when a node is reachable through multiple paths.
    #[test]
    fn minimum_depth() {
        let mut wiki = parse(concat!(
            "# Home\nSee [Left] and [Target].\n",
            "# Left\nSee [Middle].\n",
            "# Middle\nSee [Target].\n",
            "# Target",
        ))
        .unwrap();

        populate_depths(&mut wiki);

        assert_eq!(wiki.text_nodes["Left"].depth, Some(1));
        assert_eq!(wiki.text_nodes["Middle"].depth, Some(2));
        assert_eq!(wiki.text_nodes["Target"].depth, Some(1));
    }
}
