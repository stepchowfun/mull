use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::{Error, Fix, SourceRange},
    file_tree::{Visibility, file_tree_walker, visibility},
    format::CodeStr,
    line_index::LineIndex,
    spelled_path::{DirectoryListings, FileRoot, SpelledPath},
    wiki::{FilesystemTarget, HOME_TITLE, Link, Page, Wiki},
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
    // Visit pages in title order so diagnostics are deterministic.
    let mut pages = wiki.pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| &page.title);

    // Preserve graph errors if resolving the wiki later fails.
    let mut errors = validate_text_links(wiki, &pages, source_path, source_contents, line_index);

    // Report each filesystem link precisely when an editor buffer has no filesystem context.
    let Some(wiki_path) = source_path else {
        errors.extend(validate_untitled_filesystem_links(
            &pages,
            source_contents,
            line_index,
        ));
        return Outcome::Completed(errors_to_result(errors));
    };

    // Stop before touching the filesystem if the caller already lost interest in the result.
    if cancellation.is_cancelled() {
        return Outcome::Cancelled;
    }

    // Check the filesystem relative to the file root.
    let file_root = match FileRoot::new(wiki_path) {
        Ok(file_root) => file_root,
        Err(message) => {
            errors.push(Error::new(&message, Some(wiki_path), None, None, None));
            return Outcome::Completed(errors_to_result(errors));
        }
    };
    validate_filesystem_links(
        &pages,
        &file_root,
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

// Validate text-link targets and reachability from the home page, given the wiki's pages in title
// order.
fn validate_text_links(
    wiki: &Wiki,
    pages: &[&Page],
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    // Keep graph diagnostics deterministic.
    let mut errors = Vec::<Error>::new();

    // Require the root page from which every other page must be reachable, which declaring it
    // would fix.
    let has_home = wiki.pages.contains_key(HOME_TITLE);
    if !has_home {
        errors.push(Error::new(
            &format!("The wiki doesn't contain a {} page.", HOME_TITLE.code_str()),
            source_path,
            None,
            None,
            Some(Fix::CreatePage(HOME_TITLE.to_owned())),
        ));
    }

    // Validate text-link targets.
    for page in pages {
        // Report each missing target at the corresponding text-link occurrence, which declaring
        // the target would fix, unless it's empty, since no title can be. Filesystem links are
        // checked separately.
        for link in &page.links {
            match link {
                Link::Text {
                    title,
                    source_range,
                } if !wiki.pages.contains_key(title) => {
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
                            &format!("Page {} not found.", title.code_str()),
                            source_path,
                            source_context,
                            None,
                            Some(Fix::CreatePage(title.clone())),
                        )
                    });
                }
                Link::Text { .. } | Link::Filesystem { .. } => {}
            }
        }
    }

    // Reject every page outside the graph rooted at the home page.
    if has_home {
        errors.extend(
            pages
                .iter()
                .filter(|page| page.traversal_index.is_none())
                .map(|page| {
                    Error::new(
                        &format!(
                            "Page {} can't be reached by following links from {}.",
                            page.title.code_str(),
                            HOME_TITLE.code_str(),
                        ),
                        source_path,
                        Some((source_contents, line_index, page.title_source_range)),
                        None,
                        None,
                    )
                }),
        );
    }

    errors
}

// Require local filesystem context for every file and directory link in an untitled wiki, given
// its pages in title order.
fn validate_untitled_filesystem_links(
    pages: &[&Page],
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    pages
        .iter()
        .flat_map(|page| &page.links)
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

// Validate filesystem links and coverage, given the wiki's pages in title order.
fn validate_filesystem_links(
    pages: &[&Page],
    file_root: &FileRoot,
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
    for page in pages {
        for link in &page.links {
            // Stop between links so a superseded check spends no more time probing the filesystem.
            if cancellation.is_cancelled() {
                return Outcome::Cancelled;
            }

            // Skip text links, which have no filesystem targets.
            let target = match link {
                Link::Filesystem { target, .. } => target,
                Link::Text { .. } => continue,
            };
            let source_range = link.source_range();

            // Follow symbolic links when classifying each target.
            let metadata = match fs::metadata(file_root.path().join(target.path())) {
                Ok(metadata) => metadata,
                Err(error) => {
                    errors.push(inaccessible_target_error(
                        error,
                        file_root,
                        wiki_path,
                        target,
                        (source_contents, line_index, source_range),
                    ));
                    continue;
                }
            };

            // Require the path to be spelled exactly as on disk, so it matches the entries found
            // when walking the file root. A misspelled link doesn't cover its target.
            let spelled = match file_root.spell(target, &mut listings) {
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
                    file_root,
                    wiki_path,
                    target,
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
        file_root,
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

// Explain why a filesystem link's target can't be accessed. A missing target needs only the
// directory it was sought in, but any other failure keeps its underlying cause.
fn inaccessible_target_error(
    error: std::io::Error,
    file_root: &FileRoot,
    wiki_path: &Path,
    target: &FilesystemTarget,
    source_context: (&str, &LineIndex, SourceRange),
) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::new(
            &format!(
                "{} not found in {}.",
                target.code_str(),
                file_root.path().code_str(),
            ),
            Some(wiki_path),
            Some(source_context),
            None,
            None,
        )
    } else {
        Error::new(
            &format!("Unable to access {}.", target.code_str()),
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
    if target.is_directory() {
        if metadata.is_dir() {
            None
        } else if metadata.is_file() {
            Some(format!(
                "{} is a file, so its link must not end with {}.",
                target.code_str(),
                "/".code_str(),
            ))
        } else {
            Some(format!("{} isn't a directory.", target.code_str()))
        }
    } else if metadata.is_file() {
        None
    } else if metadata.is_dir() {
        Some(format!(
            "{} is a directory, so its link must end with {}.",
            target.code_str(),
            "/".code_str(),
        ))
    } else {
        Some(format!("{} isn't a file.", target.code_str()))
    }
}

// Explain why a walk of the file tree doesn't reach a filesystem link's target, or a file
// within it, where `spelled` is the target's spelling on disk.
fn visibility_error(
    file_root: &FileRoot,
    wiki_path: &Path,
    target: &FilesystemTarget,
    spelled: &SpelledPath,
    source_context: (&str, &LineIndex, SourceRange),
    cancellation: &CancellationFlag,
) -> Outcome<Option<Error>> {
    visibility(file_root, spelled, cancellation).map(|visibility| {
        let message = match visibility {
            Visibility::Visible => return None,
            Visibility::Empty => format!(
                "{} doesn't contain any files that aren't ignored.",
                target.code_str(),
            ),
            Visibility::Ignored => format!("{} is ignored.", target.code_str()),
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
    file_root: &FileRoot,
    wiki_path: &Path,
    referenced_files: &HashSet<SpelledPath>,
    referenced_directories: &HashSet<SpelledPath>,
    cancellation: &CancellationFlag,
) -> Outcome<Vec<Error>> {
    // Handle a link to the file root because the walk root bypasses the entry filter.
    if referenced_directories.iter().any(SpelledPath::is_file_root) {
        return Outcome::Completed(Vec::new());
    }

    // Require the file root to be a directory, if it exists. A missing one contains no
    // files.
    match fs::metadata(file_root.path()) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Outcome::Completed(vec![Error::new(
                &format!("{} isn't a directory.", file_root.path().code_str()),
                Some(wiki_path),
                None,
                None,
                None,
            )]);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Outcome::Completed(Vec::new());
        }
        Err(error) => {
            return Outcome::Completed(vec![Error::new(
                &format!("Unable to access {}.", file_root.path().code_str()),
                Some(wiki_path),
                None,
                Some(Arc::new(error)),
                None,
            )]);
        }
    }

    // Walk the file tree with the same visibility rules as every other filesystem consumer,
    // pruning subtrees covered by explicit directory links.
    let mut walker_builder = file_tree_walker(file_root.path());
    walker_builder.filter_entry({
        let file_root = file_root.clone();
        let referenced_directories = referenced_directories.clone();
        move |entry| !referenced_directories.contains(&file_root.entry_path(entry))
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
                    &format!("Unable to walk {}.", file_root.path().code_str()),
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
        let path = file_root.entry_path(&entry);
        if entry
            .file_type()
            .expect("Only standard input lacks a file type.")
            .is_file()
            && !referenced_files.contains(&path)
        {
            errors.push(Error::new(
                &format!("File {} isn't linked to.", path.code_str()),
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

    // This guard owns a temporary directory containing a wiki, `wiki.mull`, and removes it after a
    // test. The wiki's file root, `wiki_files`, exists only once a test puts
    // something in it.
    struct TestDirectory(PathBuf);

    // This fixture keeps parsed pages together with the source their ranges address.
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

    // Validate a fixture as the wiki at the given path.
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

    // Create an isolated directory containing a wiki without a file root.
    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mull-validation-{}-{sequence}", process::id()));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("wiki.mull"), "# Home\n").unwrap();
            Self(path)
        }

        // Expose the directory containing the wiki, which the wiki doesn't manage.
        fn path(&self) -> &Path {
            &self.0
        }

        // Expose the wiki's path.
        fn wiki_path(&self) -> PathBuf {
            self.0.join("wiki.mull")
        }

        // Locate a path within the file root, creating the directory and the path's
        // other ancestors.
        fn join(&self, path: &str) -> PathBuf {
            let path = self.0.join("wiki_files").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        }

        // Write a file within the file root.
        fn write(&self, path: &str, contents: &str) {
            fs::write(self.join(path), contents).unwrap();
        }

        // Create a directory, and any missing ancestors, within the file root.
        fn create_dir(&self, path: &str) {
            fs::create_dir_all(self.join(path)).unwrap();
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

    // Accept a wiki without a file root, which contains no files, even when other
    // files sit beside the wiki.
    #[test]
    fn missing_file_root() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unmanaged.txt"), "unmanaged").unwrap();
        fs::create_dir(directory.path().join("other")).unwrap();
        fs::write(directory.path().join("other/unmanaged.txt"), "unmanaged").unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Report filesystem links in a wiki without a file root, since their targets are
    // missing.
    #[test]
    fn links_without_file_root() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("notes.txt"), "notes").unwrap();
        let wiki = parse("# Home\n[/notes.txt] [/]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(&errors, "`/notes.txt` not found in `"));
        assert!(contains_error(&errors, "`/` not found in `"));
    }

    // Report files beside the wiki only if they're in the file root.
    #[test]
    fn only_the_file_root_is_managed() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("beside.txt"), "beside").unwrap();
        directory.write("within.txt", "within");
        let wiki = parse("# Home").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/within.txt` isn't linked to.",
        ));
    }

    // Reject a file root which is a file.
    #[test]
    fn file_root_is_a_file() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("wiki_files"), "file").unwrap();
        let wiki = parse("# Home").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(&errors, "wiki_files`"));
        assert!(contains_error(&errors, "` isn't a directory."));
    }

    // Name the file root after the wiki, replacing the usual extension, whatever its
    // case, and keeping any other.
    #[test]
    fn file_root_names() {
        for (wiki_name, directory_name) in [
            ("notes.mull", "notes_files"),
            ("notes.MULL", "notes_files"),
            ("notes", "notes_files"),
            ("notes.txt", "notes.txt_files"),
        ] {
            let directory = TestDirectory::new();
            let wiki_path = directory.path().join(wiki_name);
            fs::write(&wiki_path, "# Home\n").unwrap();
            fs::create_dir(directory.path().join(directory_name)).unwrap();
            fs::write(
                directory.path().join(directory_name).join("file.txt"),
                "file",
            )
            .unwrap();
            let wiki = parse("# Home\n[/file.txt]").unwrap();

            assert!(validate(&wiki, &wiki_path).is_ok(), "{wiki_name}");
        }
    }

    // Treat another wiki within the file root as an ordinary file.
    #[test]
    fn nested_wiki() {
        let directory = TestDirectory::new();
        directory.write("nested.mull", "# Home\n");
        directory.write("nested/file.txt", "file");
        let wiki = parse("# Home\n[/nested.mull]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/nested/file.txt` isn't linked to.",
        ));
    }

    // Validate referenced entries and prune the recursive contents of referenced directories.
    #[test]
    fn referenced_entries() {
        let directory = TestDirectory::new();
        directory.write(".gitignore", "ignored.txt\n");
        directory.write(".secret", "secret");
        directory.write("ignored.txt", "ignored");
        directory.write("images/photo.jpg", "photo");
        let wiki = parse("# Home\n[/.gitignore] [/.secret] [/images/]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Consult ignore files within the file root but not beside the wiki.
    #[test]
    fn ignore_files_beside_the_wiki() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join(".gitignore"), "notes.txt\n").unwrap();
        directory.write("notes.txt", "notes");
        let wiki = parse("# Home").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/notes.txt` isn't linked to.",
        ));
    }

    // Allow a link to the file root to cover every file in the file root.
    #[test]
    fn file_root_link() {
        let directory = TestDirectory::new();
        directory.write("notes.txt", "notes");
        directory.write("images/photo.jpg", "photo");
        let wiki = parse("# Home\n[/]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Validate against the file root when an open wiki disappears from disk.
    #[test]
    fn missing_wiki_path() {
        let directory = TestDirectory::new();
        directory.write("notes.txt", "notes");
        let wiki_path = directory.wiki_path();
        fs::remove_file(&wiki_path).unwrap();
        let wiki = parse("# Home\n[/notes.txt]").unwrap();

        assert!(validate(&wiki, &wiki_path).is_ok());
    }

    // Preserve a lexical wiki path without requiring canonicalization.
    #[test]
    fn lexical_wiki_path() {
        let directory = TestDirectory::new();
        directory.write("notes.txt", "notes");
        fs::create_dir(directory.path().join("nested")).unwrap();
        let wiki = parse("# Home\n[/notes.txt]").unwrap();

        assert!(validate(&wiki, &directory.path().join("nested/../wiki.mull")).is_ok());
    }

    // Preserve a symlinked path to the directory containing the wiki without resolving its alias.
    #[cfg(unix)]
    #[test]
    fn symlinked_wiki_path() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        directory.write("notes.txt", "notes");
        let alias_parent = TestDirectory::new();
        let alias = alias_parent.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        let wiki = parse("# Home\n[/notes.txt]").unwrap();

        assert!(validate(&wiki, &alias.join("wiki.mull")).is_ok());
    }

    // Follow a symlinked file root and validate files through its logical path.
    #[cfg(unix)]
    #[test]
    fn symlinked_file_root() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = TestDirectory::new();
        target.write("notes.txt", "notes");
        target.write("unreferenced.txt", "unreferenced");
        symlink(target.join(""), directory.path().join("wiki_files")).unwrap();
        let wiki = parse("# Home\n[/notes.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/unreferenced.txt` isn't linked to.",
        ));
    }

    // Report unreferenced files within directories instead of requiring directory links.
    #[test]
    fn unreferenced_entries() {
        let directory = TestDirectory::new();
        directory.write("images/photo.jpg", "photo");
        let wiki = parse("# Home").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/images/photo.jpg` isn't linked to.",
        ));
    }

    // Report every broken filesystem link, since the limit applies only to unreferenced files.
    #[test]
    fn filesystem_link_errors_are_unlimited() {
        let directory = TestDirectory::new();
        directory.create_dir("");
        let links = (0..=MAX_FILESYSTEM_ERRORS)
            .map(|index| format!("[/missing-{index}.txt]"))
            .collect::<Vec<_>>()
            .join(" ");
        let wiki = parse(&format!("# Home\n{links}")).unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS + 1);
        assert!(errors.iter().all(|error| {
            let message = error.to_string();
            message.contains("`/missing-") && message.contains(".txt` not found in `")
        }));
    }

    // Limit diagnostics for unreferenced files regardless of any link errors.
    #[test]
    fn unreferenced_file_error_limit() {
        let directory = TestDirectory::new();
        for index in 0..=MAX_FILESYSTEM_ERRORS {
            directory.write(&format!("unreferenced-{index}.txt"), "");
        }
        let wiki = parse("# Home\n[/missing.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS + 1);
        assert!(
            errors[0]
                .to_string()
                .contains("`/missing.txt` not found in `"),
        );
        assert!(errors[0].reason().is_none());
        assert!(
            errors[1..]
                .iter()
                .all(|error| error.to_string().contains("File `/unreferenced-")),
        );
    }

    // Infer references to directories whose files are all explicitly referenced.
    #[test]
    fn implicitly_referenced_directories() {
        let directory = TestDirectory::new();
        directory.write("notes/current.txt", "current");
        directory.write("notes/archive/old.txt", "old");
        let wiki = parse("# Home\n[/notes/current.txt] [/notes/archive/old.txt]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Consider empty directories referenced because all their contents are referenced.
    #[test]
    fn empty_directories() {
        let directory = TestDirectory::new();
        directory.create_dir("empty/nested");
        let wiki = parse("# Home").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Preserve symlink aliases as distinct filesystem paths while following their targets.
    #[cfg(unix)]
    #[test]
    fn symlink_aliases() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        directory.write("target.txt", "content");
        symlink("target.txt", directory.join("first.txt")).unwrap();
        symlink("target.txt", directory.join("second.txt")).unwrap();
        let wiki = parse("# Home\n[/target.txt] [/first.txt]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/second.txt` isn't linked to.",
        ));
    }

    // Follow an unlinked directory symlink and validate files through its logical path.
    #[cfg(unix)]
    #[test]
    fn directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        directory.write("target/file.txt", "content");
        symlink("target", directory.join("alias")).unwrap();
        let wiki = parse("# Home\n[/target/] [/alias/file.txt]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Allow directory symlinks outside the file root and validate their logical
    // contents.
    #[cfg(unix)]
    #[test]
    fn external_directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let external_directory = TestDirectory::new();
        symlink(external_directory.path(), directory.join("external")).unwrap();
        let wiki = parse("# Home\n[/external/wiki.mull]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Report a link within a directory that can't be listed, since its spelling can't be checked.
    // Permissions don't restrict a superuser, so skip this where the directory remains listable.
    #[cfg(unix)]
    #[test]
    fn unlistable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new();
        directory.write("locked/file.txt", "file");
        let locked = directory.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o311)).unwrap();
        let wiki = parse("# Home\n[/locked/file.txt]").unwrap();

        let listable = fs::read_dir(&locked).is_ok();
        let result = validate(&wiki, &directory.wiki_path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !listable {
            assert!(contains_error(
                &result.unwrap_err(),
                "Unable to list `/locked`, so the spelling of `/locked/file.txt` can't be checked.",
            ));
        }
    }

    // Report a broken symlink because its target can't be classified.
    #[cfg(unix)]
    #[test]
    fn broken_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        symlink("missing", directory.join("broken")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(contains_error(
            &validate(&wiki, &directory.wiki_path()).unwrap_err(),
            "Unable to walk `",
        ));
    }

    // Report a directory symlink cycle instead of recursing indefinitely.
    #[cfg(unix)]
    #[test]
    fn symlink_cycle() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        symlink(".", directory.join("cycle")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert!(contains_error(
            &validate(&wiki, &directory.wiki_path()).unwrap_err(),
            "Unable to walk `",
        ));
    }

    // Require links to spell paths exactly as they are on disk. A filesystem that ignores case
    // finds the target anyway, but the misspelled link doesn't cover it. A case-sensitive
    // filesystem doesn't find the target at all.
    #[test]
    fn misspelled_paths() {
        let directory = TestDirectory::new();
        directory.write("images/photo.jpg", "photo");
        let wiki = parse("# Home\n[/Images/] [/images/Photo.jpg] [/IMAGES/PHOTO.JPG]").unwrap();

        // Report the first misspelled name along each path.
        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        if fs::metadata(directory.join("IMAGES")).is_ok() {
            assert_eq!(errors.len(), 4);
            for message in [
                "`/Images` doesn't match the spelling of any name on disk.",
                "`/images/Photo.jpg` doesn't match the spelling of any name on disk.",
                "`/IMAGES` doesn't match the spelling of any name on disk.",
                "File `/images/photo.jpg` isn't linked to.",
            ] {
                assert!(contains_error(&errors, message));
            }
        } else {
            assert!(contains_error(&errors, "not found in `"));
        }
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        directory.write("images/photo.jpg", "photo");
        directory.write("notes.txt", "notes");
        let wiki = parse("# Home\n[/images] [/notes.txt/]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        for message in [
            "`/images` is a directory, so its link must end with `/`.",
            "File `/notes.txt` isn't linked to.",
            "`/notes.txt` is a file, so its link must not end with `/`.",
            "File `/images/photo.jpg` isn't linked to.",
        ] {
            assert!(contains_error(&errors, message));
        }
    }

    // Require a directory link's target to contain a file which isn't ignored, however deeply.
    #[test]
    fn directory_without_files() {
        let directory = TestDirectory::new();
        directory.write(".gitignore", "*.log\n");
        directory.create_dir("empty/nested");
        directory.write("logs/debug.log", "debug");
        directory.write("deep/nested/file.txt", "file");
        let wiki = parse("# Home\n[/.gitignore] [/empty/] [/logs/] [/deep/]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "`/empty` doesn't contain any files that aren't ignored.",
        ));
        assert!(contains_error(
            &errors,
            "`/logs` doesn't contain any files that aren't ignored.",
        ));
    }

    // Reject links to ignored files and directories, including those within ignored directories.
    #[test]
    fn ignored_targets() {
        let directory = TestDirectory::new();
        directory.write(".gitignore", "build/\nsecret.txt\n");
        directory.write("secret.txt", "secret");
        directory.write("build/output.txt", "output");
        directory.write(".git/config", "config");
        let wiki = parse(
            "# Home\n[/.gitignore] [/secret.txt] [/build/] [/build/output.txt] [/.git/config]",
        )
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        for message in [
            "`/secret.txt` is ignored.",
            "`/build` is ignored.",
            "`/build/output.txt` is ignored.",
            "`/.git/config` is ignored.",
        ] {
            assert!(contains_error(&errors, message));
        }
    }

    // Reject text links that don't correspond to any page in the wiki.
    #[test]
    fn missing_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Zulu] and [Alpha].").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(&errors, "Page `Zulu` not found."));
        assert!(contains_error(&errors, "Page `Alpha` not found."));
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
                .all(|error| error.to_string().contains("Page `Missing` not found.")),
        );
        assert_ne!(errors[0].to_string(), errors[1].to_string());
    }

    // Report independent graph, filesystem-link, and unreferenced-entry errors together.
    #[test]
    fn multiple_validation_errors() {
        let directory = TestDirectory::new();
        directory.write("unreferenced.txt", "content");
        let wiki = parse(concat!(
            "# Home\nSee [Missing] and [/missing.txt].\n",
            "# Orphan",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 4);
        for message in [
            "Page `Missing` not found.",
            "Page `Orphan` can't be reached by following links from `Home`.",
            "`/missing.txt` not found in `",
            "File `/unreferenced.txt` isn't linked to.",
        ] {
            assert!(contains_error(&errors, message));
        }
    }

    // Reject an empty text link because page titles can't be empty.
    #[test]
    fn empty_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [].").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(&errors, "This link is missing a target."));
    }

    // Require every wiki to contain its special root page.
    #[test]
    fn missing_home() {
        let directory = TestDirectory::new();
        let wiki = parse("# Elsewhere").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "The wiki doesn't contain a `Home` page.",
        ));
    }

    // Reject pages that can't be reached transitively from Home.
    #[test]
    fn unreachable_pages() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Middle].\n# Middle\n# Zulu\n# Alpha").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "Page `Alpha` can't be reached by following links from `Home`.",
        ));
        assert!(contains_error(
            &errors,
            "Page `Zulu` can't be reached by following links from `Home`.",
        ));
    }

    // Report nothing once cancellation is requested, rather than reporting a partial walk.
    #[test]
    fn cancellation_stops_filesystem_validation() {
        let directory = TestDirectory::new();
        directory.write("unreferenced.txt", "unreferenced");
        let wiki = parse("# Home").unwrap();

        // Confirm the fixture produces a filesystem error when nothing cancels the validation.
        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert!(contains_error(
            &errors,
            "File `/unreferenced.txt` isn't linked to.",
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
