use crate::{
    document::{Document, HOME_TITLE, Link},
    format::CodeStr,
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
    // Collect graph errors while populating node depths where possible.
    let mut errors = validate_text_links(document);

    // Resolve the directory and document to stable absolute paths.
    let original_document_directory = document_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let document_directory = match fs::canonicalize(original_document_directory) {
        Ok(document_directory) => document_directory,
        Err(error) => {
            errors.push(format!(
                "Failed to resolve document directory {}: {error}",
                original_document_directory.to_string_lossy().code_str(),
            ));
            return errors_to_result(&errors);
        }
    };
    let resolved_document_path = match fs::canonicalize(document_path) {
        Ok(document_path) => document_path,
        Err(error) => {
            errors.push(format!(
                "Failed to resolve {}: {error}",
                document_path.to_string_lossy().code_str(),
            ));
            return errors_to_result(&errors);
        }
    };

    // Validate filesystem links and collect their canonical targets.
    let (referenced_files, referenced_directories, filesystem_link_errors) =
        validate_filesystem_links(document, &document_directory);
    errors.extend(filesystem_link_errors);

    // Collect walk and unreferenced-entry errors for deterministic reporting.
    errors.extend(find_unreferenced_entries(
        &document_directory,
        &resolved_document_path,
        &referenced_files,
        &referenced_directories,
    ));

    // Report all validation errors together.
    errors_to_result(&errors)
}

// Validate filesystem links and collect their targets and errors.
fn validate_filesystem_links(
    document: &Document,
    document_directory: &Path,
) -> (HashSet<PathBuf>, HashSet<PathBuf>, Vec<String>) {
    // Visit nodes and links in deterministic order.
    let mut referenced_files = HashSet::<PathBuf>::new();
    let mut referenced_directories = HashSet::<PathBuf>::new();
    let mut errors = Vec::<String>::new();
    let mut nodes = document.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    for node in nodes {
        let mut links = node.links.iter().collect::<Vec<_>>();
        links.sort();
        for link in links {
            let (path, expect_directory) = match link {
                Link::Text(_) => continue,
                Link::File(path) => (path, false),
                Link::Directory(path) => (path, true),
            };

            // Retain valid targets and collect failures without stopping validation.
            match validate_target(document_directory, path, expect_directory, &node.title) {
                Ok(target) => {
                    if expect_directory {
                        referenced_directories.insert(target);
                    } else {
                        referenced_files.insert(target);
                    }
                }
                Err(error) => {
                    errors.push(error);

                    // Prevent a wrong-type link from also producing an unreferenced-entry error.
                    let target = document_directory.join(path);
                    if let Ok(metadata) = fs::metadata(&target)
                        && let Ok(target) = fs::canonicalize(target)
                    {
                        if metadata.is_file() {
                            referenced_files.insert(target);
                        } else if metadata.is_dir() {
                            referenced_directories.insert(target);
                        }
                    }
                }
            }
        }
    }

    // Return every collected target and link error.
    (referenced_files, referenced_directories, errors)
}

// Find every walked filesystem entry that is not covered by a link.
fn find_unreferenced_entries(
    document_directory: &Path,
    document_path: &Path,
    referenced_files: &HashSet<PathBuf>,
    referenced_directories: &HashSet<PathBuf>,
) -> Vec<String> {
    // Include hidden entries while retaining ignore-file behavior and excluding VCS metadata.
    let mut overrides = OverrideBuilder::new(document_directory);
    overrides
        .add("!.git/")
        .expect("the static .git override should be valid")
        .add("!.hg/")
        .expect("the static .hg override should be valid");
    let overrides = match overrides.build() {
        Ok(overrides) => overrides,
        Err(error) => return vec![format!("Failed to build filesystem ignore rules: {error}")],
    };
    let pruned_directories = referenced_directories.clone();
    let mut walker_builder = WalkBuilder::new(document_directory);
    walker_builder
        .current_dir(document_directory)
        .hidden(false)
        .parents(false)
        .require_git(false)
        .overrides(overrides)
        .filter_entry(move |entry| !pruned_directories.contains(entry.path()));

    // Collect walk and unreferenced-entry errors.
    let mut errors = Vec::<String>::new();
    for result in walker_builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("Failed to walk document directory: {error}"));
                continue;
            }
        };
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
                relative_path(document_directory, path)
                    .to_string_lossy()
                    .code_str(),
            ));
        } else if file_type.is_dir() {
            errors.push(format!(
                "Directory {} is not referenced.",
                relative_path(document_directory, path)
                    .to_string_lossy()
                    .code_str(),
            ));
        }
    }
    errors.sort();

    // Return every deterministic walk error.
    errors
}

// Validate text-link targets and the graph rooted at the special home node.
fn validate_text_links(document: &mut Document) -> Vec<String> {
    // Accumulate graph errors in deterministic order.
    let mut errors = Vec::<String>::new();

    // Require the root node from which every other node must be reachable.
    let has_home = document.text_nodes.contains_key(HOME_TITLE);
    if !has_home {
        errors.push(format!(
            "Document does not contain a {} node.",
            HOME_TITLE.code_str(),
        ));
    }

    // Validate text-link targets deterministically.
    let mut nodes = document.text_nodes.values().collect::<Vec<_>>();
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
            if !document.text_nodes.contains_key(text_link) {
                errors.push(format!(
                    "Node {} links to missing node {}.",
                    node.title.code_str(),
                    text_link.code_str(),
                ));
            }
        }
    }

    // Reset depths before finding minimum distances from the root with breadth-first traversal.
    for node in document.text_nodes.values_mut() {
        node.depth = None;
    }
    let mut pending_titles = VecDeque::<String>::new();
    if let Some(home) = document.text_nodes.get_mut(HOME_TITLE) {
        home.depth = Some(0);
        pending_titles.push_back(HOME_TITLE.to_owned());
    }
    while let Some(title) = pending_titles.pop_front() {
        let depth = document.text_nodes[&title]
            .depth
            .expect("queued nodes should have a depth");
        let text_links = document.text_nodes[&title]
            .links
            .iter()
            .filter_map(|link| match link {
                Link::Text(text_link) => Some(text_link.clone()),
                Link::File(_) | Link::Directory(_) => None,
            })
            .collect::<Vec<_>>();
        for text_link in text_links {
            if let Some(target) = document.text_nodes.get_mut(&text_link)
                && target.depth.is_none()
            {
                target.depth = Some(depth + 1);
                pending_titles.push_back(text_link);
            }
        }
    }

    // Reject every node outside the graph rooted at the home node.
    if has_home {
        let mut unreachable_titles = document
            .text_nodes
            .values()
            .filter(|node| node.depth.is_none())
            .map(|node| &node.title)
            .collect::<Vec<_>>();
        unreachable_titles.sort();
        errors.extend(unreachable_titles.into_iter().map(|title| {
            format!(
                "Node {} is not reachable from {}.",
                title.code_str(),
                HOME_TITLE.code_str(),
            )
        }));
    }

    // Return every text-link graph error.
    errors
}

// Convert collected validation errors into the public result type.
fn errors_to_result(errors: &[String]) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
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
            "Node {} links to inaccessible path {}: {error}",
            node_title.code_str(),
            path.to_string_lossy().code_str(),
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
            "Node {} links to {}, which is not a {expected_type}.",
            node_title.code_str(),
            path.to_string_lossy().code_str(),
        ));
    }

    // Canonicalize the validated target for comparison with walked entries.
    fs::canonicalize(&target).map_err(|error| {
        format!(
            "Failed to resolve path {} linked from node {}: {error}",
            path.to_string_lossy().code_str(),
            node_title.code_str(),
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

    // Preserve graph errors when the document path cannot be resolved.
    #[test]
    fn missing_document_path() {
        let directory = TestDirectory::new();
        let document_path = directory.document_path();
        fs::remove_file(&document_path).unwrap();
        let mut document = parse("# Elsewhere").unwrap();

        let error = validate(&mut document, &document_path).unwrap_err();
        let mut errors = error.lines();
        assert_eq!(
            errors.next(),
            Some("Document does not contain a `Home` node."),
        );
        assert!(
            errors
                .next()
                .unwrap()
                .starts_with(&format!("Failed to resolve `{}`:", document_path.display())),
        );
        assert_eq!(errors.next(), None);
    }

    // Report unreferenced entries throughout the document directory tree.
    #[test]
    fn unreferenced_entries() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let mut document = parse("# Home").unwrap();

        let error = validate(&mut document, &directory.document_path()).unwrap_err();
        assert!(error.contains("Directory `images` is not referenced."));
        assert!(error.contains(&format!(
            "`{}`",
            Path::new("images").join("photo.jpg").display(),
        )));
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        let mut document = parse(concat!("# Home\n[", "file:images]")).unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Node `Home` links to `images`, which is not a file.",
        );
    }

    // Reject text links that do not correspond to any node in the document.
    #[test]
    fn missing_text_link() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [Zulu] and [Alpha].").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            concat!(
                "Node `Home` links to missing node `Alpha`.\n",
                "Node `Home` links to missing node `Zulu`.",
            ),
        );
    }

    // Report independent graph, filesystem-link, and unreferenced-entry errors together.
    #[test]
    fn multiple_validation_errors() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unreferenced.txt"), "content").unwrap();
        let mut document = parse(concat!(
            "# Home\nSee [Missing] and [",
            "file:missing.txt].\n",
            "# Orphan",
        ))
        .unwrap();

        let error = validate(&mut document, &directory.document_path()).unwrap_err();
        assert!(error.contains("Node `Home` links to missing node `Missing`."));
        assert!(error.contains("Node `Orphan` is not reachable from `Home`."));
        assert!(error.contains("Node `Home` links to inaccessible path `missing.txt`:"));
        assert!(error.contains("File `unreferenced.txt` is not referenced."));
        assert_eq!(error.lines().count(), 4);
    }

    // Reject an empty text link because node titles cannot be empty.
    #[test]
    fn empty_text_link() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [].").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Node `Home` links to missing node ``.",
        );
    }

    // Require every document to contain its special root node.
    #[test]
    fn missing_home() {
        let directory = TestDirectory::new();
        let mut document = parse("# Elsewhere").unwrap();

        assert_eq!(
            validate(&mut document, &directory.document_path()).unwrap_err(),
            "Document does not contain a `Home` node.",
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
                "Node `Alpha` is not reachable from `Home`.\n",
                "Node `Zulu` is not reachable from `Home`.",
            ),
        );
    }

    // Populate depths for nodes reached through multiple levels of text links.
    #[test]
    fn transitive_text_links() {
        let directory = TestDirectory::new();
        let mut document = parse("# Home\nSee [Middle].\n# Middle\nSee [End].\n# End").unwrap();

        assert_eq!(validate(&mut document, &directory.document_path()), Ok(()));
        assert_eq!(document.text_nodes["Home"].depth, Some(0));
        assert_eq!(document.text_nodes["Middle"].depth, Some(1));
        assert_eq!(document.text_nodes["End"].depth, Some(2));
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
        assert_eq!(document.text_nodes["Left"].depth, Some(1));
        assert_eq!(document.text_nodes["Middle"].depth, Some(2));
        assert_eq!(document.text_nodes["Target"].depth, Some(1));
    }
}
