use crate::wiki::{HOME_TITLE, Link, Wiki};
use std::collections::BTreeSet;

// Populate the position of each text node in a depth-first traversal of text links from the home
// node, visiting the targets of each node's links in title order.
pub fn populate_traversal_order(wiki: &mut Wiki) {
    // Reset positions before recalculating them.
    for node in wiki.text_nodes.values_mut() {
        node.traversal_index = None;
    }

    // Start the traversal at the home node, if there is one.
    let mut pending_titles = Vec::<String>::new();
    if wiki.text_nodes.contains_key(HOME_TITLE) {
        pending_titles.push(HOME_TITLE.to_owned());
    }

    // Number each node when it's first popped, so the numbering follows a depth-first preorder.
    let mut next_index = 0;
    while let Some(title) = pending_titles.pop() {
        // Skip nodes which were already reached through another path.
        let node = wiki
            .text_nodes
            .get_mut(&title)
            .expect("Queued titles should refer to existing nodes.");
        if node.traversal_index.is_some() {
            continue;
        }
        node.traversal_index = Some(next_index);
        next_index += 1;

        // Push unvisited link targets in reverse title order so they're popped in title order.
        let text_links = node
            .links
            .iter()
            .filter_map(|link| match link {
                Link::Text { title, .. } => Some(title.clone()),
                Link::Filesystem { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        for text_link in text_links.into_iter().rev() {
            if wiki
                .text_nodes
                .get(&text_link)
                .is_some_and(|target| target.traversal_index.is_none())
            {
                pending_titles.push(text_link);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::populate_traversal_order;
    use crate::{parser::parse, wiki::Wiki};
    use std::path::Path;

    // Parse a wiki and return its reachable titles in traversal order.
    fn traversal_order(source: &str) -> (Wiki, Vec<String>) {
        let mut wiki = parse(Some(Path::new("test.mull")), source).unwrap();
        populate_traversal_order(&mut wiki);
        let mut nodes = wiki
            .text_nodes
            .values()
            .filter_map(|node| {
                node.traversal_index
                    .map(|index| (index, node.title.clone()))
            })
            .collect::<Vec<_>>();
        nodes.sort();
        let titles = nodes.into_iter().map(|(_, title)| title).collect();
        (wiki, titles)
    }

    // Visit each subtree completely before moving on to the next sibling.
    #[test]
    fn depth_first_preorder() {
        let (_, titles) = traversal_order(concat!(
            "# Home\nSee [A] and [B].\n",
            "# A\nSee [A1] and [A2].\n",
            "# A1\nSee [A1a].\n",
            "# A1a\n",
            "# A2\n",
            "# B\nSee [B1].\n",
            "# B1\n",
        ));

        assert_eq!(titles, vec!["Home", "A", "A1", "A1a", "A2", "B", "B1"]);
    }

    // Visit siblings in title order regardless of the order of the links.
    #[test]
    fn siblings_in_title_order() {
        let (_, titles) = traversal_order(concat!(
            "# Home\nSee [Charlie], [Alpha], [Bravo], and [Alpha] again.\n",
            "# Alpha\n",
            "# Bravo\n",
            "# Charlie\n",
        ));

        assert_eq!(titles, vec!["Home", "Alpha", "Bravo", "Charlie"]);
    }

    // Place a node reachable through multiple paths at its first visit.
    #[test]
    fn first_visit_wins() {
        let (_, titles) = traversal_order(concat!(
            "# Home\nSee [Left] and [Target].\n",
            "# Left\nSee [Middle].\n",
            "# Middle\nSee [Target].\n",
            "# Target\nSee [Home] and [Left].\n",
        ));

        assert_eq!(titles, vec!["Home", "Left", "Middle", "Target"]);
    }

    // Leave nodes which can't be reached from the home node without a position.
    #[test]
    fn unreachable_nodes() {
        let (wiki, titles) = traversal_order("# Home\nSee [Greeting].\n# Greeting\n# Orphan\n");

        assert_eq!(titles, vec!["Home", "Greeting"]);
        assert_eq!(wiki.text_nodes["Orphan"].traversal_index, None);
    }
}
