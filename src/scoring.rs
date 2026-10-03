use crate::wiki::{HOME_TITLE, Link, TextNode, Wiki};
use std::collections::{BTreeSet, HashMap, VecDeque};

// Populate the position of each text node in a depth-first traversal from the home node which only
// follows links along shortest paths, visiting the targets of each node's links in title order.
// Each node thus appears after the first of its closest-to-home parents, keeping its minimum
// distance from the home node.
pub fn populate_traversal_order(wiki: &mut Wiki) {
    // Reset positions before recalculating them.
    for node in wiki.text_nodes.values_mut() {
        node.traversal_index = None;
    }

    // Leave every node unreachable if there's no home node.
    if !wiki.text_nodes.contains_key(HOME_TITLE) {
        return;
    }

    // Compute minimum text-link distances from the home node using breadth-first traversal.
    let mut depths = HashMap::<String, usize>::from([(HOME_TITLE.to_owned(), 0)]);
    let mut queued_titles = VecDeque::from([HOME_TITLE.to_owned()]);
    while let Some(title) = queued_titles.pop_front() {
        let depth = depths[&title];
        for target in text_link_titles(&wiki.text_nodes[&title]) {
            if wiki.text_nodes.contains_key(&target) && !depths.contains_key(&target) {
                depths.insert(target.clone(), depth + 1);
                queued_titles.push_back(target);
            }
        }
    }

    // Number each node when it's first popped, so the numbering follows a depth-first preorder.
    let mut pending_titles = vec![HOME_TITLE.to_owned()];
    let mut next_index = 0;
    while let Some(title) = pending_titles.pop() {
        // Skip nodes which were already reached through another parent.
        let node = wiki
            .text_nodes
            .get_mut(&title)
            .expect("Pending titles should refer to existing nodes.");
        if node.traversal_index.is_some() {
            continue;
        }
        node.traversal_index = Some(next_index);
        next_index += 1;

        // Push unvisited targets one link farther from home in reverse title order so they're
        // popped in title order.
        let child_depth = depths[&title] + 1;
        for target in text_link_titles(node).into_iter().rev() {
            if depths.get(&target) == Some(&child_depth)
                && wiki.text_nodes[&target].traversal_index.is_none()
            {
                pending_titles.push(target);
            }
        }
    }
}

// Collect the distinct titles of a node's text links in title order.
fn text_link_titles(node: &TextNode) -> BTreeSet<String> {
    node.links
        .iter()
        .filter_map(|link| match link {
            Link::Text { title, .. } => Some(title.clone()),
            Link::Filesystem { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::populate_traversal_order;
    use crate::{line_index::LineIndex, parser::parse, wiki::Wiki};
    use std::path::Path;

    // Parse a wiki and return its reachable titles in traversal order.
    fn traversal_order(source: &str) -> (Wiki, Vec<String>) {
        let (mut wiki, errors) = parse(
            Some(Path::new("test.mull")),
            source,
            &LineIndex::new(source),
        );
        assert!(errors.is_empty());
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

    // Place a node directly after its closest-to-home parent rather than at the end of a longer
    // path that happens to be visited first.
    #[test]
    fn shortest_paths_only() {
        let (_, titles) = traversal_order(concat!(
            "# Home\nSee [A], [M], and [Z].\n",
            "# A\nSee [B].\n",
            "# B\nSee [Z].\n",
            "# M\n",
            "# Z\n",
        ));

        assert_eq!(titles, vec!["Home", "A", "B", "M", "Z"]);
    }

    // Place a node after the first visited of its parents which are equally close to home.
    #[test]
    fn first_parent_at_same_depth() {
        let (_, titles) = traversal_order(concat!(
            "# Home\nSee [B] and [A].\n",
            "# A\nSee [X].\n",
            "# B\nSee [X].\n",
            "# X\n",
        ));

        assert_eq!(titles, vec!["Home", "A", "X", "B"]);
    }

    // Place a node reachable through multiple paths only once, even with cycles.
    #[test]
    fn multiple_paths_and_cycles() {
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
