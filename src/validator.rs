use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::{Error, Fix, SourceRange},
    format::{CodePath, CodeStr},
    line_index::LineIndex,
    spelled_path::{DirectoryListings, SpelledPath, WikiDirectory},
    wiki::{FilesystemTarget, HOME_TITLE, Link, TextNode, Wiki},
    wiki_tree::{Visibility, visibility, wiki_tree_walker},
};
use std::{collections::HashSet, fs, path::Path, sync::Arc};

// Limit diagnostics for unreferenced files so pathological directories remain manageable.
const MAX_FILESYSTEM_ERRORS: usize = 50;

// Check text links, reachability, filesystem links, and filesystem coverage.
pub fn validate(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    cancellation: &CancellationFlag,
) -> Outcome<Result<(), Vec<Error>>> {
    // Visit nodes in title order so diagnostics are deterministic.
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);

    // Preserve graph errors if resolving the wiki later fails.
    let mut errors = validate_text_links(wiki, &nodes, source_path, source_contents, line_index);

    // Report each filesystem link precisely when an editor buffer has no filesystem context.
    let Some(wiki_path) = source_path else {
        errors.extend(validate_untitled_filesystem_links(
            &nodes,
            source_contents,
            line_index,
        ));
        return Outcome::Completed(errors_to_result(errors));
    };

    // Stop before touching the filesystem if the caller already lost interest in the result.
    if cancellation.is_cancelled() {
        return Outcome::Cancelled;
    }

    // Check the filesystem relative to the wiki's containing directory, which requires the wiki's
    // own path to be spelled as on disk.
    let wiki_directory = match WikiDirectory::new(wiki_path) {
        Ok(wiki_directory) => wiki_directory,
        Err(error) => {
            errors.push(Error::new(
                &error.message,
                Some(wiki_path),
                None,
                error.reason,
                None,
            ));
            return Outcome::Completed(errors_to_result(errors));
        }
    };
    validate_filesystem_links(
        &nodes,
        &wiki_directory,
        wiki_path,
        source_contents,
        line_index,
        cancellation,
    )
    .map(|filesystem_errors| {
        errors.extend(filesystem_errors);
        errors_to_result(errors)
    })
}

// Validate text-link targets and reachability from the home node, given the wiki's nodes in title
// order.
fn validate_text_links(
    wiki: &Wiki,
    nodes: &[&TextNode],
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    // Keep graph diagnostics deterministic.
    let mut errors = Vec::<Error>::new();

    // Require the root node from which every other node must be reachable, which declaring it
    // would fix.
    let has_home = wiki.text_nodes.contains_key(HOME_TITLE);
    if !has_home {
        errors.push(Error::new(
            &format!("The wiki doesn't contain a {} node.", HOME_TITLE.code_str()),
            source_path,
            None,
            None,
            Some(Fix::CreateNode(HOME_TITLE.to_owned())),
        ));
    }

    // Validate text-link targets.
    for node in nodes {
        // Report each missing target at the corresponding text-link occurrence, which declaring
        // the target would fix, unless it's empty, since no title can be. Filesystem links are
        // checked separately.
        for link in &node.links {
            match link {
                Link::Text {
                    title,
                    source_range,
                } if !wiki.text_nodes.contains_key(title) => {
                    let source_context = Some((source_contents, line_index, *source_range));
                    errors.push(if title.is_empty() {
                        Error::new(
                            "This link is missing a target.",
                            source_path,
                            source_context,
                            None,
                            None,
                        )
                    } else {
                        Error::new(
                            &format!("Node {} not found.", title.code_str()),
                            source_path,
                            source_context,
                            None,
                            Some(Fix::CreateNode(title.clone())),
                        )
                    });
                }
                Link::Text { .. } | Link::Filesystem { .. } => {}
            }
        }
    }

    // Reject every node outside the graph rooted at the home node.
    if has_home {
        errors.extend(
            nodes
                .iter()
                .filter(|node| node.traversal_index.is_none())
                .map(|node| {
                    Error::new(
                        &format!(
                            "Node {} can't be reached by following links from {}.",
                            node.title.code_str(),
                            HOME_TITLE.code_str(),
                        ),
                        source_path,
                        Some((source_contents, line_index, node.title_source_range)),
                        None,
                        None,
                    )
                }),
        );
    }

    errors
}

// Require local filesystem context for every file and directory link in an untitled wiki, given
// its nodes in title order.
fn validate_untitled_filesystem_links(
    nodes: &[&TextNode],
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    nodes
        .iter()
        .flat_map(|node| &node.links)
        .filter_map(|link| match link {
            Link::Filesystem { source_range, .. } => Some(Error::new(
                "Save the wiki to validate this filesystem link.",
                None,
                Some((source_contents, line_index, *source_range)),
                None,
                None,
            )),
            Link::Text { .. } => None,
        })
        .collect()
}

// Validate filesystem links and coverage, given the wiki's nodes in title order.
fn validate_filesystem_links(
    nodes: &[&TextNode],
    wiki_directory: &WikiDirectory,
    wiki_path: &Path,
    source_contents: &str,
    line_index: &LineIndex,
    cancellation: &CancellationFlag,
) -> Outcome<Vec<Error>> {
    // Track valid targets while visiting links.
    let mut referenced_files = HashSet::<SpelledPath>::new();
    let mut referenced_directories = HashSet::<SpelledPath>::new();
    let mut listings = DirectoryListings::new();
    let mut errors = Vec::<Error>::new();
    for node in nodes {
        for link in &node.links {
            // Stop between links so a superseded check spends no more time probing the filesystem.
            if cancellation.is_cancelled() {
                return Outcome::Cancelled;
            }

            // Skip text links, which have no filesystem targets.
            let target = match link {
                Link::Filesystem { target, .. } => target,
                Link::Text { .. } => continue,
            };
            let (path, source_range) = (target.path(), link.source_range());

            // Follow symbolic links when classifying each target.
            let metadata = match fs::metadata(wiki_directory.path().join(path)) {
                Ok(metadata) => metadata,
                Err(error) => {
                    errors.push(inaccessible_target_error(
                        error,
                        wiki_path,
                        path,
                        (source_contents, line_index, source_range),
                    ));
                    continue;
                }
            };

            // Require the path to be spelled exactly as on disk, so it matches the entries found
            // when walking the wiki's directory. A misspelled link doesn't cover its target.
            let spelled = match wiki_directory.spell(target, &mut listings) {
                Ok(spelled) => Some(spelled),
                Err(error) => {
                    errors.push(Error::new(
                        &error.message,
                        Some(wiki_path),
                        Some((source_contents, line_index, source_range)),
                        error.reason,
                        None,
                    ));
                    None
                }
            };

            // Report links with the wrong type.
            if let Some(message) = wrong_target_type_message(target, &metadata) {
                errors.push(Error::new(
                    &message,
                    Some(wiki_path),
                    Some((source_contents, line_index, source_range)),
                    None,
                    None,
                ));
                continue;
            }

            // Require the target to be a file which isn't ignored, or a directory containing such a
            // file. Skip this if the path's spelling wasn't confirmed, which was already reported.
            if let Some(spelled) = &spelled {
                match visibility_error(
                    wiki_directory,
                    wiki_path,
                    path,
                    spelled,
                    (source_contents, line_index, source_range),
                    cancellation,
                ) {
                    Outcome::Completed(error) => errors.extend(error),
                    Outcome::Cancelled => return Outcome::Cancelled,
                }
            }

            // Track the target so the walk for unreferenced files accounts for it.
            if let Some(spelled) = spelled {
                if target.is_directory() {
                    referenced_directories.insert(spelled);
                } else {
                    referenced_files.insert(spelled);
                }
            }
        }
    }

    // Report uncovered filesystem entries.
    find_unreferenced_filesystem_links(
        wiki_directory,
        wiki_path,
        &referenced_files,
        &referenced_directories,
        cancellation,
    )
    .map(|unreferenced_errors| {
        errors.extend(unreferenced_errors);
        errors
    })
}

// Explain why a filesystem link's target can't be accessed. A missing target needs no further
// explanation, but any other failure keeps its underlying cause.
fn inaccessible_target_error(
    error: std::io::Error,
    wiki_path: &Path,
    path: &Path,
    source_context: (&str, &LineIndex, SourceRange),
) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::new(
            &format!("{} not found.", path.code_path()),
            Some(wiki_path),
            Some(source_context),
            None,
            None,
        )
    } else {
        Error::new(
            &format!("Unable to access {}.", path.code_path()),
            Some(wiki_path),
            Some(source_context),
            Some(Arc::new(error)),
            None,
        )
    }
}

// Explain why a filesystem link's target has the wrong type, if it does, suggesting a change to the
// link's trailing `/` only when that change would fix the link.
fn wrong_target_type_message(target: &FilesystemTarget, metadata: &fs::Metadata) -> Option<String> {
    let path = target.path();
    if target.is_directory() {
        if metadata.is_dir() {
            None
        } else if metadata.is_file() {
            Some(format!(
                "{} is a file, so its link must not end with {}.",
                path.code_path(),
                "/".code_str(),
            ))
        } else {
            Some(format!("{} isn't a directory.", path.code_path()))
        }
    } else if metadata.is_file() {
        None
    } else if metadata.is_dir() {
        Some(format!(
            "{} is a directory, so its link must end with {}.",
            path.code_path(),
            "/".code_str(),
        ))
    } else {
        Some(format!("{} isn't a file.", path.code_path()))
    }
}

// Explain why a walk of the wiki tree doesn't reach a filesystem link's target, or a file within
// it, where `path` is the link's path and `spelled` is its spelling on disk.
fn visibility_error(
    wiki_directory: &WikiDirectory,
    wiki_path: &Path,
    path: &Path,
    spelled: &SpelledPath,
    source_context: (&str, &LineIndex, SourceRange),
    cancellation: &CancellationFlag,
) -> Outcome<Option<Error>> {
    visibility(wiki_directory, spelled, cancellation).map(|visibility| {
        let message = match visibility {
            Visibility::Visible => return None,
            Visibility::Empty => format!(
                "{} doesn't contain any files that aren't ignored.",
                path.code_path(),
            ),
            Visibility::Ignored => format!("{} is ignored.", path.code_path()),
        };
        Some(Error::new(
            &message,
            Some(wiki_path),
            Some(source_context),
            None,
            None,
        ))
    })
}

// Find unreferenced files, up to a limit so pathological directories remain manageable, while
// pruning covered directories.
fn find_unreferenced_filesystem_links(
    wiki_directory: &WikiDirectory,
    wiki_path: &Path,
    referenced_files: &HashSet<SpelledPath>,
    referenced_directories: &HashSet<SpelledPath>,
    cancellation: &CancellationFlag,
) -> Outcome<Vec<Error>> {
    // Handle a link to the wiki directory because the walk root bypasses the entry filter.
    if referenced_directories
        .iter()
        .any(SpelledPath::is_wiki_directory)
    {
        return Outcome::Completed(Vec::new());
    }

    // Walk the wiki tree with the same visibility rules as every other filesystem consumer, pruning
    // subtrees covered by explicit directory links.
    let mut walker_builder = wiki_tree_walker(wiki_directory.path());
    walker_builder.filter_entry({
        let wiki_directory = wiki_directory.clone();
        let referenced_directories = referenced_directories.clone();
        move |entry| {
            // Exclude the wiki and prune directories already covered by their links.
            let path = wiki_directory.entry_path(entry);
            wiki_directory.wiki_path() != Some(&path) && !referenced_directories.contains(&path)
        }
    });

    // Stop traversing once the error limit is reached.
    let mut errors = Vec::<Error>::new();
    for result in walker_builder.build() {
        // Stop between entries so a superseded check doesn't walk the rest of the tree.
        if cancellation.is_cancelled() {
            return Outcome::Cancelled;
        }

        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(Error::new(
                    "Unable to walk the wiki directory.",
                    Some(wiki_path),
                    None,
                    Some(Arc::new(error)),
                    None,
                ));
                if errors.len() >= MAX_FILESYSTEM_ERRORS {
                    break;
                }
                continue;
            }
        };
        let path = wiki_directory.entry_path(&entry);
        if entry
            .file_type()
            .expect("Only standard input lacks a file type.")
            .is_file()
            && !referenced_files.contains(&path)
        {
            errors.push(Error::new(
                &format!("File {} isn't linked to.", path.code_path()),
                Some(wiki_path),
                None,
                None,
                None,
            ));
            if errors.len() >= MAX_FILESYSTEM_ERRORS {
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
        line_index::LineIndex,
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
        line_index: LineIndex,
    }

    // Parse a test wiki while retaining its source for validation listings, rejecting sources with
    // syntax errors.
    fn parse(source_contents: &str) -> Result<TestWiki, Vec<Error>> {
        let line_index = LineIndex::new(source_contents);
        let (wiki, errors) = parse_wiki(Some(Path::new("test.mull")), source_contents, &line_index);
        if errors.is_empty() {
            Ok(TestWiki {
                wiki,
                source_contents: source_contents.to_owned(),
                line_index,
            })
        } else {
            Err(errors)
        }
    }

    // Validate a fixture using a stable display path for deterministic diagnostics.
    fn validate(wiki: &TestWiki, wiki_path: &Path) -> Result<(), Vec<Error>> {
        validate_wiki(
            &wiki.wiki,
            Some(wiki_path),
            &wiki.source_contents,
            &wiki.line_index,
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
            &wiki.line_index,
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
        let source = "# Home\n[/notes.txt] [/images/]";
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
            vec!["[/notes.txt]", "[/images/]"],
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
        let wiki = parse("# Home\n[/.gitignore] [/.secret] [/images/]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Allow a wiki-directory link to cover every surrounding filesystem entry.
    #[test]
    fn wiki_directory_link() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unmanaged.txt"), "content").unwrap();
        let wiki = parse("# Home\n[/]").unwrap();

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

    // Reject the wiki's own path spelled differently than on disk, which only a filesystem that
    // ignores case finds.
    #[test]
    fn misspelled_wiki_path() {
        let directory = TestDirectory::new();
        let wiki_path = directory.path().join("WIKI.mull");
        let wiki = parse("# Home").unwrap();

        if fs::metadata(&wiki_path).is_ok() {
            assert!(contains_error(
                &validate(&wiki, &wiki_path).unwrap_err(),
                "`WIKI.mull` doesn't match the spelling of any name on disk.",
            ));
        }
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
        assert!(
            errors[0]
                .to_string()
                .contains(&format!("File `{}` isn't linked to.", photo_path.display())),
        );
    }

    // Report every broken filesystem link, since the limit applies only to unreferenced files.
    #[test]
    fn filesystem_link_errors_are_unlimited() {
        let directory = TestDirectory::new();
        let links = (0..=MAX_FILESYSTEM_ERRORS)
            .map(|index| format!("[/missing-{index}.txt]"))
            .collect::<Vec<_>>()
            .join(" ");
        let wiki = parse(&format!("# Home\n{links}")).unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS + 1);
        assert!(errors.iter().all(|error| {
            let message = error.to_string();
            message.contains("`missing-") && message.contains(".txt` not found.")
        }));
    }

    // Limit diagnostics for unreferenced files regardless of any link errors.
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
        let wiki = parse("# Home\n[/missing.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS + 1);
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
        let wiki = parse("# Home\n[/notes/current.txt] [/notes/archive/old.txt]").unwrap();

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
        let wiki = parse("# Home\n[/target.txt] [/first.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `second.txt` isn't linked to.",
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
        let wiki = parse("# Home\n[/target/] [/alias/file.txt]").unwrap();

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
        let wiki = parse("# Home\n[/external/wiki.mull]").unwrap();

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
        fs::write(&target_path, "# Home\n[/wiki.txt]").unwrap();
        symlink("wiki.txt", &wiki_path).unwrap();
        let wiki = parse("# Home\n[/wiki.txt]").unwrap();

        assert!(validate(&wiki, &wiki_path).is_ok());
    }

    // Report a link within a directory that can't be listed, since its spelling can't be checked.
    // Permissions don't restrict a superuser, so skip this where the directory remains listable.
    #[cfg(unix)]
    #[test]
    fn unlistable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new();
        let locked = directory.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("file.txt"), "file").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o311)).unwrap();
        let wiki = parse("# Home\n[/locked/file.txt]").unwrap();

        let listable = fs::read_dir(&locked).is_ok();
        let result = validate(&wiki, &directory.wiki_path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !listable {
            let file_path = Path::new("locked").join("file.txt");
            assert!(contains_error(
                &result.unwrap_err(),
                &format!(
                    "Unable to list `locked`, so the spelling of `{}` can't be checked.",
                    file_path.display(),
                ),
            ));
        }
    }

    // Report a broken symlink because its target can't be classified.
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
                .any(|error| {
                    error
                        .to_string()
                        .contains("Unable to walk the wiki directory.")
                }),
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
                .any(|error| {
                    error
                        .to_string()
                        .contains("Unable to walk the wiki directory.")
                }),
        );
    }

    // Require links to spell paths exactly as they are on disk. A filesystem that ignores case
    // finds the target anyway, but the misspelled link doesn't cover it. A case-sensitive
    // filesystem doesn't find the target at all.
    #[test]
    fn misspelled_paths() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let wiki = parse("# Home\n[/Images/] [/images/Photo.jpg] [/IMAGES/PHOTO.JPG]").unwrap();

        // Report the first misspelled name along each path.
        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        if fs::metadata(directory.path().join("IMAGES")).is_ok() {
            let photo_path = Path::new("images").join("photo.jpg");
            let misspelled_photo_path = Path::new("images").join("Photo.jpg");
            assert_eq!(errors.len(), 4);
            for message in [
                "`Images` doesn't match the spelling of any name on disk.".to_owned(),
                format!(
                    "`{}` doesn't match the spelling of any name on disk.",
                    misspelled_photo_path.display(),
                ),
                "`IMAGES` doesn't match the spelling of any name on disk.".to_owned(),
                format!("File `{}` isn't linked to.", photo_path.display()),
            ] {
                assert!(contains_error(&errors, &message));
            }
        } else {
            assert!(contains_error(&errors, "not found."));
        }
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        fs::write(directory.path().join("notes.txt"), "notes").unwrap();
        let wiki = parse("# Home\n[/images] [/notes.txt/]").unwrap();
        let photo_path = Path::new("images").join("photo.jpg");

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        assert!(contains_error(
            &errors,
            "`images` is a directory, so its link must end with `/`.",
        ));
        assert!(contains_error(&errors, "File `notes.txt` isn't linked to."));
        assert!(contains_error(
            &errors,
            "`notes.txt` is a file, so its link must not end with `/`.",
        ));
        assert!(contains_error(
            &errors,
            &format!("File `{}` isn't linked to.", photo_path.display()),
        ));
    }

    // Require a directory link's target to contain a file which isn't ignored, however deeply.
    #[test]
    fn directory_without_files() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join(".gitignore"), "*.log\n").unwrap();
        fs::create_dir_all(directory.path().join("empty/nested")).unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        fs::write(directory.path().join("logs/debug.log"), "debug").unwrap();
        fs::create_dir_all(directory.path().join("deep/nested")).unwrap();
        fs::write(directory.path().join("deep/nested/file.txt"), "file").unwrap();
        let wiki = parse("# Home\n[/.gitignore] [/empty/] [/logs/] [/deep/]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "`empty` doesn't contain any files that aren't ignored.",
        ));
        assert!(contains_error(
            &errors,
            "`logs` doesn't contain any files that aren't ignored.",
        ));
    }

    // Reject links to ignored files and directories, including those within ignored directories.
    #[test]
    fn ignored_targets() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join(".gitignore"), "build/\nsecret.txt\n").unwrap();
        fs::write(directory.path().join("secret.txt"), "secret").unwrap();
        fs::create_dir(directory.path().join("build")).unwrap();
        fs::write(directory.path().join("build/output.txt"), "output").unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(directory.path().join(".git/config"), "config").unwrap();
        let wiki = parse(
            "# Home\n[/.gitignore] [/secret.txt] [/build/] [/build/output.txt] [/.git/config]",
        )
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        let output_path = Path::new("build").join("output.txt");
        let config_path = Path::new(".git").join("config");
        assert_eq!(errors.len(), 4);
        assert!(contains_error(&errors, "`secret.txt` is ignored."));
        assert!(contains_error(&errors, "`build` is ignored."));
        assert!(contains_error(
            &errors,
            &format!("`{}` is ignored.", output_path.display()),
        ));
        assert!(contains_error(
            &errors,
            &format!("`{}` is ignored.", config_path.display()),
        ));
    }

    // Reject text links that don't correspond to any node in the wiki.
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
            "# Home\nSee [Missing] and [/missing.txt].\n",
            "# Orphan",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        assert!(contains_error(&errors, "Node `Missing` not found."));
        assert!(contains_error(
            &errors,
            "Node `Orphan` can't be reached by following links from `Home`.",
        ));
        assert!(contains_error(&errors, "`missing.txt` not found."));
        assert!(contains_error(
            &errors,
            "File `unreferenced.txt` isn't linked to.",
        ));
    }

    // Reject an empty text link because node titles can't be empty.
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
            "The wiki doesn't contain a `Home` node.",
        ));
    }

    // Reject nodes that can't be reached transitively from Home.
    #[test]
    fn unreachable_nodes() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Middle].\n# Middle\n# Zulu\n# Alpha").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "Node `Alpha` can't be reached by following links from `Home`.",
        ));
        assert!(contains_error(
            &errors,
            "Node `Zulu` can't be reached by following links from `Home`.",
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
            "File `unreferenced.txt` isn't linked to.",
        ));

        // Request cancellation before validating the same fixture again.
        let cancellation = CancellationFlag::default();
        cancellation.cancel();
        let outcome = validate_wiki(
            &wiki.wiki,
            Some(directory.wiki_path().as_path()),
            &wiki.source_contents,
            &wiki.line_index,
            &cancellation,
        );
        assert!(matches!(outcome, Outcome::Cancelled));
    }
}
