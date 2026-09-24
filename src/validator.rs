use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::Error,
    format::{CodePath, CodeStr},
    path_util::relative_path,
    wiki::{HOME_TITLE, Link, Wiki},
    wiki_tree::wiki_tree_walker,
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

// Limit filesystem diagnostics so pathological wikis and directories remain manageable.
const MAX_FILESYSTEM_ERRORS: usize = 50;

// Check text links, reachability, filesystem links, and filesystem coverage.
pub fn validate(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    cancellation: &CancellationFlag,
) -> Outcome<Result<(), Vec<Error>>> {
    // Preserve graph errors if resolving the wiki later fails.
    let mut errors = validate_text_links(wiki, source_path, source_contents);

    // Report each filesystem link precisely when an editor buffer has no filesystem context.
    let Some(wiki_path) = source_path else {
        errors.extend(validate_untitled_filesystem_links(wiki, source_contents));
        return Outcome::Completed(errors_to_result(errors));
    };

    // Stop before touching the filesystem if the caller already lost interest in the result.
    if cancellation.is_cancelled() {
        return Outcome::Cancelled;
    }

    // Derive every filesystem path from the wiki's containing directory.
    let wiki_directory = wiki_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Check the filesystem relative to the resolved wiki directory.
    let Outcome::Completed(filesystem_errors) = validate_filesystem_links(
        wiki,
        wiki_directory,
        wiki_path,
        source_contents,
        cancellation,
    ) else {
        return Outcome::Cancelled;
    };
    errors.extend(filesystem_errors);
    Outcome::Completed(errors_to_result(errors))
}

// Validate text-link targets and reachability from the home node.
fn validate_text_links(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
) -> Vec<Error> {
    // Keep graph diagnostics deterministic.
    let mut errors = Vec::<Error>::new();

    // Require the root node from which every other node must be reachable.
    let has_home = wiki.text_nodes.contains_key(HOME_TITLE);
    if !has_home {
        errors.push(Error::new(
            &format!(
                "The wiki does not contain a {} node.",
                HOME_TITLE.code_str(),
            ),
            source_path,
            None,
            None,
        ));
    }

    // Validate text-link targets deterministically.
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    for node in &nodes {
        // Report each missing target at the corresponding text-link occurrence.
        for link in &node.links {
            if let Link::Text {
                title,
                source_range,
            } = link
                && !wiki.text_nodes.contains_key(title)
            {
                let message = if title.is_empty() {
                    "This link is missing a target.".to_owned()
                } else {
                    format!("Node {} not found.", title.code_str())
                };
                errors.push(Error::new(
                    &message,
                    source_path,
                    Some((source_contents, *source_range)),
                    None,
                ));
            }
        }
    }

    // Reject every node outside the graph rooted at the home node.
    if has_home {
        let mut unreachable_titles = wiki
            .text_nodes
            .values()
            .filter(|node| node.depth.is_none())
            .map(|node| (&node.title, node.title_source_range))
            .collect::<Vec<_>>();
        unreachable_titles.sort_by_key(|(title, _source_range)| *title);
        errors.extend(unreachable_titles.into_iter().map(|(title, source_range)| {
            Error::new(
                &format!(
                    "Node {} is not reachable from {}.",
                    title.code_str(),
                    HOME_TITLE.code_str(),
                ),
                source_path,
                Some((source_contents, source_range)),
                None,
            )
        }));
    }

    errors
}

// Require local filesystem context for every file and directory link in an untitled wiki.
fn validate_untitled_filesystem_links(wiki: &Wiki, source_contents: &str) -> Vec<Error> {
    // Visit links deterministically and respect the shared filesystem-error budget.
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    nodes
        .into_iter()
        .flat_map(|node| &node.links)
        .filter_map(|link| match link {
            Link::File { source_range, .. } | Link::Directory { source_range, .. } => {
                Some(Error::new(
                    "Save the wiki to validate this filesystem link.",
                    None,
                    Some((source_contents, *source_range)),
                    None,
                ))
            }
            Link::Text { .. } => None,
        })
        .take(MAX_FILESYSTEM_ERRORS)
        .collect()
}

// Validate filesystem links and coverage within a bounded error budget.
fn validate_filesystem_links(
    wiki: &Wiki,
    wiki_directory: &Path,
    wiki_path: &Path,
    source_contents: &str,
    cancellation: &CancellationFlag,
) -> Outcome<Vec<Error>> {
    // Track valid targets while visiting nodes and links in deterministic order.
    let mut referenced_files = HashSet::<PathBuf>::new();
    let mut referenced_directories = HashSet::<PathBuf>::new();
    let mut errors = Vec::<Error>::new();
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    'nodes: for node in nodes {
        for link in &node.links {
            // Stop between links so a superseded check spends no more time probing the filesystem.
            if cancellation.is_cancelled() {
                return Outcome::Cancelled;
            }

            // Skip text links before processing filesystem links [tag:filesystem_links_only].
            let (path, source_range) = match link {
                Link::Text { .. } => continue,
                Link::File { path, source_range } | Link::Directory { path, source_range } => {
                    (path, *source_range)
                }
            };

            // Follow symbolic links when classifying each target.
            let target = wiki_directory.join(path);
            let metadata = match fs::metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) => {
                    // A missing target needs no further explanation, but any other failure keeps
                    // its underlying cause.
                    errors.push(if error.kind() == std::io::ErrorKind::NotFound {
                        Error::new(
                            &format!("{} not found.", path.code_path()),
                            Some(wiki_path),
                            Some((source_contents, source_range)),
                            None,
                        )
                    } else {
                        Error::new(
                            &format!("Unable to access {}.", path.code_path()),
                            Some(wiki_path),
                            Some((source_contents, source_range)),
                            Some(Rc::new(error)),
                        )
                    });
                    if errors.len() >= MAX_FILESYSTEM_ERRORS {
                        break 'nodes;
                    }
                    continue;
                }
            };

            // Retain correctly typed targets and report links with the wrong type.
            match link {
                Link::File { .. } if metadata.is_file() => {
                    referenced_files.insert(target);
                }
                Link::Directory { .. } if metadata.is_dir() => {
                    referenced_directories.insert(target);
                }
                Link::File { .. } if metadata.is_dir() => errors.push(Error::new(
                    &format!(
                        "{} is a directory, so its link must end with {}.",
                        path.code_path(),
                        "/".code_str(),
                    ),
                    Some(wiki_path),
                    Some((source_contents, source_range)),
                    None,
                )),
                Link::File { .. } => errors.push(Error::new(
                    &format!("{} is not a file.", path.code_path()),
                    Some(wiki_path),
                    Some((source_contents, source_range)),
                    None,
                )),
                Link::Directory { .. } => errors.push(Error::new(
                    &format!(
                        "{} is not a directory, so its link must not end with {}.",
                        path.code_path(),
                        "/".code_str(),
                    ),
                    Some(wiki_path),
                    Some((source_contents, source_range)),
                    None,
                )),
                Link::Text { .. } => {
                    // Text links were skipped above [ref:filesystem_links_only].
                    unreachable!("text links were already skipped")
                }
            }
            if errors.len() >= MAX_FILESYSTEM_ERRORS {
                break 'nodes;
            }
        }
    }

    // Avoid a directory walk when link validation exhausted the error budget.
    if errors.len() >= MAX_FILESYSTEM_ERRORS {
        return Outcome::Completed(errors);
    }

    // Spend the remaining error budget on uncovered filesystem entries.
    let remaining_error_capacity = MAX_FILESYSTEM_ERRORS - errors.len();
    let Outcome::Completed(unreferenced_errors) = find_unreferenced_filesystem_links(
        wiki_directory,
        wiki_path,
        &referenced_files,
        &referenced_directories,
        remaining_error_capacity,
        cancellation,
    ) else {
        return Outcome::Cancelled;
    };
    errors.extend(unreferenced_errors);

    Outcome::Completed(errors)
}

// Find unreferenced files within a budget while pruning covered directories.
fn find_unreferenced_filesystem_links(
    wiki_directory: &Path,
    wiki_path: &Path,
    referenced_files: &HashSet<PathBuf>,
    referenced_directories: &HashSet<PathBuf>,
    maximum_errors: usize,
    cancellation: &CancellationFlag,
) -> Outcome<Vec<Error>> {
    // Handle a link to the wiki directory because the walk root bypasses the entry filter.
    if referenced_directories.contains(wiki_directory) {
        return Outcome::Completed(Vec::new());
    }

    // Walk the wiki tree with the same visibility rules as every other filesystem consumer.
    let mut walker_builder = match wiki_tree_walker(wiki_directory) {
        Ok(walker_builder) => walker_builder,
        Err(error) => {
            return Outcome::Completed(vec![Error::new(
                "Unable to build filesystem ignore rules.",
                Some(wiki_path),
                None,
                Some(Rc::new(error)),
            )]);
        }
    };

    // Prune subtrees covered by explicit directory links.
    walker_builder.filter_entry({
        let wiki_directory = wiki_directory.to_owned();
        let relative_wiki_path = relative_path(&wiki_directory, wiki_path).to_owned();
        let referenced_directories = referenced_directories.clone();
        move |entry| {
            // Exclude the wiki and prune directories already covered by their links.
            relative_path(&wiki_directory, entry.path()) != relative_wiki_path
                && !referenced_directories.contains(entry.path())
        }
    });

    // Stop traversing once the remaining error budget is exhausted.
    let mut errors = Vec::<Error>::new();
    for result in walker_builder.build() {
        // Stop between entries so a superseded check does not walk the rest of the tree.
        if cancellation.is_cancelled() {
            return Outcome::Cancelled;
        }

        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(Error::new(
                    "Unable to walk wiki directory.",
                    Some(wiki_path),
                    None,
                    Some(Rc::new(error)),
                ));
                if errors.len() >= maximum_errors {
                    break;
                }
                continue;
            }
        };
        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && !referenced_files.contains(path) {
            errors.push(Error::new(
                &format!(
                    "File {} is not referenced.",
                    relative_path(wiki_directory, path).code_path(),
                ),
                Some(wiki_path),
                None,
                None,
            ));
            if errors.len() >= maximum_errors {
                break;
            }
        }
    }

    // Present the collected walk errors in deterministic order.
    errors.sort_by_key(ToString::to_string);
    Outcome::Completed(errors)
}

// Convert collected validation errors into the public result type.
fn errors_to_result(errors: Vec<Error>) -> Result<(), Vec<Error>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_FILESYSTEM_ERRORS, validate as validate_wiki};
    use crate::{
        cancellation::{CancellationFlag, Outcome},
        error::Error,
        parser::parse as parse_wiki,
        wiki::Wiki,
    };
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

    // This fixture keeps parsed nodes together with the source their ranges address.
    struct TestWiki {
        wiki: Wiki,
        source_contents: String,
    }

    // Parse a test wiki while retaining its source for validation listings.
    fn parse(source_contents: &str) -> Result<TestWiki, Vec<Error>> {
        parse_wiki(Some(Path::new("test.mull")), source_contents).map(|wiki| TestWiki {
            wiki,
            source_contents: source_contents.to_owned(),
        })
    }

    // Validate a fixture using a stable display path for deterministic diagnostics.
    fn validate(wiki: &TestWiki, wiki_path: &Path) -> Result<(), Vec<Error>> {
        validate_wiki(
            &wiki.wiki,
            Some(wiki_path),
            &wiki.source_contents,
            &CancellationFlag::default(),
        )
        .assume_completed()
    }

    // Validate a fixture without pretending that its editor buffer has a filesystem path.
    fn validate_untitled(wiki: &TestWiki) -> Result<(), Vec<Error>> {
        validate_wiki(
            &wiki.wiki,
            None,
            &wiki.source_contents,
            &CancellationFlag::default(),
        )
        .assume_completed()
    }

    // Check rendered diagnostics without coupling tests to their complete listings.
    fn contains_error(errors: &[Error], message: &str) -> bool {
        errors
            .iter()
            .any(|error| error.to_string().contains(message))
    }

    // Create an isolated directory containing a wiki.
    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mull-validation-{}-{sequence}", process::id()));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("wiki.mull"), "# Home\n").unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn wiki_path(&self) -> PathBuf {
            self.0.join("wiki.mull")
        }
    }

    // Clean up files created by a validation test.
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    // Validate syntax and graph structure without requiring an untitled wiki to be saved.
    #[test]
    fn untitled_text_only_wiki() {
        let wiki = parse("# Home\nSee [Greeting].\n# Greeting").unwrap();

        assert!(validate_untitled(&wiki).is_ok());
    }

    // Report every filesystem link at its source location until an untitled wiki is saved.
    #[test]
    fn untitled_filesystem_links() {
        let source = "# Home\n[./notes.txt] [./images/]";
        let wiki = parse(source).unwrap();

        let errors = validate_untitled(&wiki).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|error| {
            error.message() == "Save the wiki to validate this filesystem link."
                && error.source_path().is_none()
        }));
        assert_eq!(
            errors
                .iter()
                .map(|error| {
                    let range = error.source_range().unwrap();
                    &source[range.start..range.end]
                })
                .collect::<Vec<_>>(),
            vec!["[./notes.txt]", "[./images/]"],
        );
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
        let wiki = parse("# Home\n[./.gitignore] [./.secret] [./images/]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Allow a wiki-directory link to cover every surrounding filesystem entry.
    #[test]
    fn wiki_directory_link() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unmanaged.txt"), "content").unwrap();
        let wiki = parse("# Home\n[./]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Validate against the containing directory when an open wiki disappears from disk.
    #[test]
    fn missing_wiki_path() {
        let directory = TestDirectory::new();
        let wiki_path = directory.wiki_path();
        fs::remove_file(&wiki_path).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &wiki_path).is_ok());
    }

    // Preserve a lexical containing-directory path without requiring canonicalization.
    #[test]
    fn lexical_wiki_directory() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("nested")).unwrap();
        let wiki_path = directory.path().join("nested/../wiki.mull");
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &wiki_path).is_ok());
    }

    // Preserve a symlinked containing-directory path without resolving its alias.
    #[cfg(unix)]
    #[test]
    fn symlinked_wiki_directory() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let alias_parent = TestDirectory::new();
        let alias = alias_parent.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &alias.join("wiki.mull")).is_ok());
    }

    // Report unreferenced files within directories instead of requiring directory links.
    #[test]
    fn unreferenced_entries() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let wiki = parse("# Home").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        let photo_path = Path::new("images").join("photo.jpg");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains(&format!(
            "File `{}` is not referenced.",
            photo_path.display(),
        )));
    }

    // Stop validating explicit filesystem links after reaching the diagnostic limit.
    #[test]
    fn filesystem_link_error_limit() {
        let directory = TestDirectory::new();
        let links = (0..=MAX_FILESYSTEM_ERRORS)
            .map(|index| format!("[./missing-{index}.txt]"))
            .collect::<Vec<_>>()
            .join(" ");
        let wiki = parse(&format!("# Home\n{links}")).unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS);
        assert!(errors.iter().all(|error| {
            let message = error.to_string();
            message.contains("`missing-") && message.contains(".txt` not found.")
        }));
    }

    // Share the diagnostic limit between link validation and the filesystem walk.
    #[test]
    fn unreferenced_file_error_limit() {
        let directory = TestDirectory::new();
        for index in 0..=MAX_FILESYSTEM_ERRORS {
            fs::write(
                directory.path().join(format!("unreferenced-{index}.txt")),
                "",
            )
            .unwrap();
        }
        let wiki = parse("# Home\n[./missing.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS);
        assert!(errors[0].to_string().contains("`missing.txt` not found."));
        assert!(errors[0].reason().is_none());
        assert!(
            errors[1..]
                .iter()
                .all(|error| error.to_string().contains("File `unreferenced-")),
        );
    }

    // Infer references to directories whose files are all explicitly referenced.
    #[test]
    fn implicitly_referenced_directories() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("notes")).unwrap();
        fs::create_dir(directory.path().join("notes/archive")).unwrap();
        fs::write(directory.path().join("notes/current.txt"), "current").unwrap();
        fs::write(directory.path().join("notes/archive/old.txt"), "old").unwrap();
        let wiki = parse("# Home\n[./notes/current.txt] [./notes/archive/old.txt]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Consider empty directories referenced because all their contents are referenced.
    #[test]
    fn empty_directories() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("empty")).unwrap();
        fs::create_dir(directory.path().join("empty/nested")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Preserve symlink aliases as distinct filesystem paths while following their targets.
    #[cfg(unix)]
    #[test]
    fn symlink_aliases() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        fs::write(directory.path().join("target.txt"), "content").unwrap();
        symlink("target.txt", directory.path().join("first.txt")).unwrap();
        symlink("target.txt", directory.path().join("second.txt")).unwrap();
        let wiki = parse("# Home\n[./target.txt] [./first.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `second.txt` is not referenced.",
        ));
    }

    // Follow an unlinked directory symlink and validate files through its logical path.
    #[cfg(unix)]
    #[test]
    fn directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("target")).unwrap();
        fs::write(directory.path().join("target/file.txt"), "content").unwrap();
        symlink("target", directory.path().join("alias")).unwrap();
        let wiki = parse("# Home\n[./target/] [./alias/file.txt]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Allow directory symlinks outside the wiki tree and validate their logical contents.
    #[cfg(unix)]
    #[test]
    fn external_directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let external_directory = TestDirectory::new();
        symlink(external_directory.path(), directory.path().join("external")).unwrap();
        let wiki = parse("# Home\n[./external/wiki.mull]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Exclude a wiki symlink by its logical path instead of its resolved target.
    #[cfg(unix)]
    #[test]
    fn wiki_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let wiki_path = directory.wiki_path();
        let target_path = directory.path().join("wiki.txt");
        fs::rename(&wiki_path, &target_path).unwrap();
        fs::write(&target_path, "# Home\n[./wiki.txt]").unwrap();
        symlink("wiki.txt", &wiki_path).unwrap();
        let wiki = parse("# Home\n[./wiki.txt]").unwrap();

        assert!(validate(&wiki, &wiki_path).is_ok());
    }

    // Report a broken symlink because its target cannot be classified.
    #[cfg(unix)]
    #[test]
    fn broken_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        symlink("missing", directory.path().join("broken")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(
            validate(&wiki, &directory.wiki_path())
                .unwrap_err()
                .iter()
                .any(|error| error.to_string().contains("Unable to walk wiki directory.")),
        );
    }

    // Report a directory symlink cycle instead of recursing indefinitely.
    #[cfg(unix)]
    #[test]
    fn symlink_cycle() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        symlink(".", directory.path().join("cycle")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(
            validate(&wiki, &directory.wiki_path())
                .unwrap_err()
                .iter()
                .any(|error| error.to_string().contains("Unable to walk wiki directory.")),
        );
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        fs::write(directory.path().join("notes.txt"), "notes").unwrap();
        let wiki = parse("# Home\n[./images] [./notes.txt/]").unwrap();
        let photo_path = Path::new("images").join("photo.jpg");

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        assert!(contains_error(
            &errors,
            "`images` is a directory, so its link must end with `/`.",
        ));
        assert!(contains_error(
            &errors,
            "File `notes.txt` is not referenced.",
        ));
        assert!(contains_error(
            &errors,
            "`notes.txt` is not a directory, so its link must not end with `/`.",
        ));
        assert!(contains_error(
            &errors,
            &format!("File `{}` is not referenced.", photo_path.display()),
        ));
    }

    // Reject text links that do not correspond to any node in the wiki.
    #[test]
    fn missing_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Zulu] and [Alpha].").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(&errors, "Node `Zulu` not found."));
        assert!(contains_error(&errors, "Node `Alpha` not found."));
    }

    // Report repeated invalid links at each distinct source occurrence.
    #[test]
    fn repeated_missing_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Missing] and [Missing].").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("Node `Missing` not found.")),
        );
        assert_ne!(errors[0].to_string(), errors[1].to_string());
    }

    // Report independent graph, filesystem-link, and unreferenced-entry errors together.
    #[test]
    fn multiple_validation_errors() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unreferenced.txt"), "content").unwrap();
        let wiki = parse(concat!(
            "# Home\nSee [Missing] and [./missing.txt].\n",
            "# Orphan",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        assert!(contains_error(&errors, "Node `Missing` not found."));
        assert!(contains_error(
            &errors,
            "Node `Orphan` is not reachable from `Home`.",
        ));
        assert!(contains_error(&errors, "`missing.txt` not found."));
        assert!(contains_error(
            &errors,
            "File `unreferenced.txt` is not referenced.",
        ));
    }

    // Reject an empty text link because node titles cannot be empty.
    #[test]
    fn empty_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [].").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(&errors, "This link is missing a target."));
    }

    // Require every wiki to contain its special root node.
    #[test]
    fn missing_home() {
        let directory = TestDirectory::new();
        let wiki = parse("# Elsewhere").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "The wiki does not contain a `Home` node.",
        ));
    }

    // Reject nodes that cannot be reached transitively from Home.
    #[test]
    fn unreachable_nodes() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Middle].\n# Middle\n# Zulu\n# Alpha").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "Node `Alpha` is not reachable from `Home`.",
        ));
        assert!(contains_error(
            &errors,
            "Node `Zulu` is not reachable from `Home`.",
        ));
    }

    // Report nothing once cancellation is requested, rather than reporting a partial walk.
    #[test]
    fn cancellation_stops_filesystem_validation() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unreferenced.txt"), "unreferenced").unwrap();
        let wiki = parse("# Home").unwrap();

        // Confirm the fixture produces a filesystem error when nothing cancels the validation.
        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert!(contains_error(
            &errors,
            "File `unreferenced.txt` is not referenced.",
        ));

        // Request cancellation before validating the same fixture again.
        let cancellation = CancellationFlag::default();
        cancellation.cancel();
        let outcome = validate_wiki(
            &wiki.wiki,
            Some(directory.wiki_path().as_path()),
            &wiki.source_contents,
            &cancellation,
        );
        assert!(matches!(outcome, Outcome::Cancelled));
    }
}
