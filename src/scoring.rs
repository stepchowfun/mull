use crate::wiki::{HOME_TITLE, Link, Page, Wiki};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

// Populate the position of each page in the traversal order, leaving unreachable pages
// without one.
pub fn populate_traversal_order(wiki: &mut Wiki) {
    // Compute the order before updating the pages it borrows from.
    let order = traversal_order(wiki, None)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    // Reset positions, then number the reachable pages.
    for page in wiki.pages.values_mut() {
        page.traversal_index = None;
    }
    for (index, title) in order.into_iter().enumerate() {
        wiki.pages
            .get_mut(&title)
            .expect("Traversed titles should refer to existing pages.")
            .traversal_index = Some(index);
    }
}

// List the titles of the pages reachable from the home page in the order of a depth-first
// traversal which only follows links along shortest paths, visiting the targets of each page's
// links in title order. Each page thus appears after the first of its closest-to-home parents,
// keeping its minimum distance from the home page. The traversal can include an extra page without
// links, as if it existed, which doesn't change the order of the others.
pub fn traversal_order<'a>(wiki: &'a Wiki, extra_title: Option<&'a str>) -> Vec<&'a str> {
    // Treat the extra page as an existing page with no links.
    let exists = |title: &str| wiki.pages.contains_key(title) || extra_title == Some(title);
    let link_titles = |title: &str| {
        wiki.pages
            .get(title)
            .map(text_link_titles)
            .unwrap_or_default()
    };

    // Leave every page unreachable if there's no home page.
    if !exists(HOME_TITLE) {
        return Vec::new();
    }

    // Compute minimum text-link distances from the home page using breadth-first traversal.
    let mut depths = HashMap::<&str, usize>::from([(HOME_TITLE, 0)]);
    let mut queued_titles = VecDeque::from([HOME_TITLE]);
    while let Some(title) = queued_titles.pop_front() {
        let depth = depths[title];
        for target in link_titles(title) {
            if exists(target) && !depths.contains_key(target) {
                depths.insert(target, depth + 1);
                queued_titles.push_back(target);
            }
        }
    }

    // List each page when it's first popped, so the order follows a depth-first preorder.
    let mut order = Vec::new();
    let mut visited_titles = HashSet::new();
    let mut pending_titles = vec![HOME_TITLE];
    while let Some(title) = pending_titles.pop() {
        // Skip pages which were already reached through another parent.
        if !visited_titles.insert(title) {
            continue;
        }
        order.push(title);

        // Push unvisited targets one link farther from home in reverse title order so they're
        // popped in title order.
        let child_depth = depths[title] + 1;
        for target in link_titles(title).into_iter().rev() {
            if depths.get(target) == Some(&child_depth) && !visited_titles.contains(target) {
                pending_titles.push(target);
            }
        }
    }
    order
}

// Collect the distinct titles of a page's text links in title order.
fn text_link_titles(page: &Page) -> BTreeSet<&str> {
    page.links
        .iter()
        .filter_map(|link| match link {
            Link::Text { title, .. } => Some(title.as_str()),
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
        let mut pages = wiki
            .pages
            .values()
            .filter_map(|page| {
                page.traversal_index
                    .map(|index| (index, page.title.clone()))
            })
            .collect::<Vec<_>>();
        pages.sort();
        let titles = pages.into_iter().map(|(_, title)| title).collect();
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

    // Place a page directly after its closest-to-home parent rather than at the end of a longer
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

    // Place a page after the first visited of its parents which are equally close to home.
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

    // Place a page reachable through multiple paths only once, even with cycles.
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

    // Leave pages which can't be reached from the home page without a position.
    #[test]
    fn unreachable_pages() {
        let (wiki, titles) = traversal_order("# Home\nSee [Greeting].\n# Greeting\n# Orphan\n");

        assert_eq!(titles, vec!["Home", "Greeting"]);
        assert_eq!(wiki.pages["Orphan"].traversal_index, None);
    }
}
