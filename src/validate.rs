use crate::{
    document::{Document, Link},
    path_util::relative_path,
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

// Validate every link and ensure every walked filesystem entry is referenced.
pub fn validate(document: &mut Document, document_path: &Path) -> Result<(), String> {
    // Validate the graph formed by nodes and their text links.
    validate_text_links(document)?;

    // Resolve the directory and document to stable absolute paths.
    let document_directory = document_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let document_directory = fs::canonicalize(document_directory).map_err(|error| {
        format!(
            "Failed to resolve document directory {}: {error}",
            document_directory.display(),
        )
    })?;
    let document_path = fs::canonicalize(document_path)
        .map_err(|error| format!("Failed to resolve {}: {error}", document_path.display()))?;

    // Validate filesystem links and collect their canonical targets.
    let mut referenced_files = HashSet::<PathBuf>::new();
    let mut referenced_directories = HashSet::<PathBuf>::new();
    let mut nodes = document.nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    for node in nodes {
        let mut links = node.links.iter().collect::<Vec<_>>();
        links.sort();
        for link in links {
            match link {
                Link::Text(_) => {}
                Link::File(path) => {
                    let target = validate_target(&document_directory, path, false, &node.title)?;
                    referenced_files.insert(target);
                }
                Link::Directory(path) => {
                    let target = validate_target(&document_directory, path, true, &node.title)?;
                    referenced_directories.insert(target);
                }
            }
        }
    }

    // Include hidden entries while retaining ignore-file behavior and excluding VCS metadata.
    let mut overrides = OverrideBuilder::new(&document_directory);
    overrides
        .add("!.git/")
        .expect("the static .git override should be valid")
        .add("!.hg/")
        .expect("the static .hg override should be valid");
    let overrides = overrides
        .build()
        .map_err(|error| format!("Failed to build filesystem ignore rules: {error}"))?;
    let pruned_directories = referenced_directories.clone();
    let mut walker_builder = WalkBuilder::new(&document_directory);
    walker_builder
        .current_dir(&document_directory)
        .hidden(false)
        .parents(false)
        .require_git(false)
        .overrides(overrides)
        .filter_entry(move |entry| !pruned_directories.contains(entry.path()));
    let walker = walker_builder.build();

    // Collect every unreferenced file and directory for deterministic reporting.
    let mut errors = Vec::<String>::new();
    for result in walker {
        let entry =
            result.map_err(|error| format!("Failed to walk document directory: {error}"))?;
        let path = entry.path();
        if path == document_directory || path == document_path {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && !referenced_files.contains(path) {
            errors.push(format!(
                "File {} is not referenced.",
                relative_path(&document_directory, path).display(),
            ));
        } else if file_type.is_dir() {
            errors.push(format!(
                "Directory {} is not referenced.",
                relative_path(&document_directory, path).display(),
            ));
        }
    }
    errors.sort();

    // Report every unreferenced entry together.
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

// Validate text-link targets and the graph rooted at Home.
fn validate_text_links(document: &mut Document) -> Result<(), String> {
    // Require the root node from which every other node must be reachable.
    if !document.nodes.contains_key("Home") {
        return Err("Document does not contain a \"Home\" node.".to_owned());
    }

    // Validate text links deterministically after every node is available.
    let mut nodes = document.nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    for node in &nodes {
        let mut text_links = node
            .links
            .iter()
            .filter_map(|link| match link {
                Link::Text(title) => Some(title),
                Link::File(_) | Link::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        text_links.sort();
        for text_link in text_links {
            if !document.nodes.contains_key(text_link) {
                return Err(format!(
                    "Node {:?} links to missing node {text_link:?}.",
                    node.title,
                ));
            }
        }
    }

    // Reset depths before finding minimum distances from Home with breadth-first traversal.
    for node in document.nodes.values_mut() {
        node.depth = None;
    }
    document
        .nodes
        .get_mut("Home")
        .expect("Home should already be present")
        .depth = Some(0);
    let mut pending_titles = VecDeque::from(["Home".to_owned()]);
    while let Some(title) = pending_titles.pop_front() {
        let depth = document.nodes[&title]
            .depth
            .expect("queued nodes should have a depth");
        let text_links = document.nodes[&title]
            .links
            .iter()
            .filter_map(|link| match link {
                Link::Text(text_link) => Some(text_link.clone()),
                Link::File(_) | Link::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        for text_link in text_links {
            let target = document
                .nodes
                .get_mut(&text_link)
                .expect("text-link targets should already be validated");
            if target.depth.is_none() {
                target.depth = Some(depth + 1);
                pending_titles.push_back(text_link);
            }
        }
    }

    // Reject every node outside the graph rooted at Home.
    let mut unreachable_titles = document
        .nodes
        .values()
        .filter(|node| node.depth.is_none())
        .map(|node| &node.title)
        .collect::<Vec<_>>();
    unreachable_titles.sort();
    if !unreachable_titles.is_empty() {
        return Err(unreachable_titles
            .into_iter()
            .map(|title| format!("Node {title:?} is not reachable from \"Home\"."))
            .collect::<Vec<_>>()
            .join("\n"));
    }

    // The text-link graph is valid.
    Ok(())
}

// Validate one filesystem target and return its canonical path.
fn validate_target(
    document_directory: &Path,
    path: &Path,
    expect_directory: bool,
    node_title: &str,
) -> Result<PathBuf, String> {
    // Resolve the link relative to the document directory.
    let target = document_directory.join(path);
    let metadata = fs::metadata(&target).map_err(|error| {
        format!(
            "Node {node_title:?} links to inaccessible path {}: {error}",
            path.display(),
        )
    })?;
    let has_expected_type = if expect_directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if !has_expected_type {
        let expected_type = if expect_directory {
            "directory"
        } else {
            "file"
        };
        return Err(format!(
            "Node {node_title:?} links to {}, which is not a {expected_type}.",
            path.display(),
        ));
    }

    // Canonicalize the validated target for comparison with walked entries.
    fs::canonicalize(&target).map_err(|error| {
        format!(
            "Failed to resolve path {} linked from node {node_title:?}: {error}",
            path.display(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::validate;
    use crate::parse::parse;
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
    };

    // Assign each test directory a unique path even when tests run concurrently.
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    // This guard owns a temporary directory and removes it after a test.
    struct TestDirectory(PathBuf);

    // Create an isolated directory containing a document.
    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mull-validation-{}-{sequence}", process::id()));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("document.mull"), "# Home\n").unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn document_path(&self) -> PathBuf {
            self.0.join("document.mull")
        }
    }

    // Clean up files created by a validation test.
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    // Validate referenced entries and prune the recursive contents of referenced directories.
    #[test]
    fn referenced_entries() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.path().join(".secret"), "secret").unwrap();
        fs::write(directory.path().join("ignored.txt"), "ignored").unwrap();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let mut document = parse(concat!(
            "# Home\n[",
            "file:.gitignore] [",
            "file:.secret] [",
            "dir:images]",
        ))
        .unwrap();

        assert_eq!(validate(&mut document, &directory.document_path()), Ok(()));
    }

    // Report unreferenced entries throughout the document directory tree.
    #[test]
    fn unreferenced_entries() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let mut document = parse("# Home").unwrap();

        let error = validate(&mut document, &directory.document_path()).unwrap_err();
        assert!(error.contains("Directory images is not referenced."));
        assert!(error.contains("photo.jpg"));
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        let mut document = parse(concat!("# Home\n[", "file:images]")).unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Node \"Home\" links to images, which is not a file.",
        );
    }

    // Reject text links that do not correspond to any node in the document.
    #[test]
    fn missing_text_link() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [Zulu] and [Alpha].").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Node \"Home\" links to missing node \"Alpha\".",
        );
    }

    // Reject an empty text link because node titles cannot be empty.
    #[test]
    fn empty_text_link() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [].").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Node \"Home\" links to missing node \"\".",
        );
    }

    // Require every document to contain its special root node.
    #[test]
    fn missing_home() {
        let directory = TestDirectory::new();
        let mut document = parse("# Elsewhere").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Document does not contain a \"Home\" node.",
        );
    }

    // Reject nodes that cannot be reached transitively from Home.
    #[test]
    fn unreachable_nodes() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [Middle].\n# Middle\n# Zulu\n# Alpha").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            concat!(
                "Node \"Alpha\" is not reachable from \"Home\".\n",
                "Node \"Zulu\" is not reachable from \"Home\".",
            ),
        );
    }

    // Accept nodes reached through multiple levels of text links.
    #[test]
    fn transitive_text_links() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [Middle].\n# Middle\nSee [End].\n# End").unwrap();

        assert_eq!(validate(&mut document, &directory.document_path()), Ok(()));
        assert_eq!(document.nodes["Home"].depth, Some(0));
        assert_eq!(document.nodes["Middle"].depth, Some(1));
        assert_eq!(document.nodes["End"].depth, Some(2));
    }

    // Choose the shortest distance when a node is reachable through multiple paths.
    #[test]
    fn minimum_depth() {
        let directory = TestDirectory::new();
        let mut document = parse(concat!(
            "# Home\nSee [Left] and [Target].\n",
            "# Left\nSee [Middle].\n",
            "# Middle\nSee [Target].\n",
            "# Target",
        ))
        .unwrap();

        assert_eq!(validate(&mut document, &directory.document_path()), Ok(()));
        assert_eq!(document.nodes["Left"].depth, Some(1));
        assert_eq!(document.nodes["Middle"].depth, Some(2));
        assert_eq!(document.nodes["Target"].depth, Some(1));
    }
}
