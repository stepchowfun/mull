use crate::{
    Errors,
    format::{CodePath, CodeStr},
    path_util::relative_path,
    wiki::{HOME_TITLE, Link, Wiki},
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

// Limit filesystem diagnostics so pathological wikis and directories remain manageable.
const MAX_FILESYSTEM_ERRORS: usize = 50;

// Check text links, reachability, filesystem links, and filesystem coverage.
pub fn validate(wiki: &Wiki, wiki_path: &Path) -> Result<(), Errors> {
    // Preserve graph errors if resolving the wiki later fails.
    let mut errors = validate_text_links(wiki);

    // Resolve the directory to a stable path and confirm that the wiki is accessible.
    let original_wiki_directory = wiki_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let wiki_directory = match fs::canonicalize(original_wiki_directory) {
        Ok(wiki_directory) => wiki_directory,
        Err(error) => {
            errors.push(format!(
                "Failed to resolve wiki directory {}: {error}",
                original_wiki_directory.code_path(),
            ));
            return errors_to_result(errors);
        }
    };
    if let Err(error) = fs::metadata(wiki_path) {
        errors.push(format!(
            "Failed to resolve {}: {error}",
            wiki_path.code_path(),
        ));
        return errors_to_result(errors);
    }
    let Some(wiki_file_name) = wiki_path.file_name() else {
        errors.push(format!(
            "Failed to determine the file name of {}.",
            wiki_path.code_path(),
        ));
        return errors_to_result(errors);
    };
    let logical_wiki_path = wiki_directory.join(wiki_file_name);

    // Check the filesystem relative to the resolved wiki directory.
    errors.extend(validate_filesystem_links(
        wiki,
        &wiki_directory,
        &logical_wiki_path,
    ));
    errors_to_result(errors)
}

// Validate text-link targets and reachability from the home node.
fn validate_text_links(wiki: &Wiki) -> Vec<String> {
    // Keep graph diagnostics deterministic.
    let mut errors = Vec::<String>::new();

    // Require the root node from which every other node must be reachable.
    let has_home = wiki.text_nodes.contains_key(HOME_TITLE);
    if !has_home {
        errors.push(format!(
            "Wiki does not contain a {} node.",
            HOME_TITLE.code_str(),
        ));
    }

    // Validate text-link targets deterministically.
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
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
            if !wiki.text_nodes.contains_key(text_link) {
                errors.push(format!(
                    "Node {} links to missing node {}.",
                    node.title.code_str(),
                    text_link.code_str(),
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

    errors
}

// Validate filesystem links and coverage within a bounded error budget.
fn validate_filesystem_links(wiki: &Wiki, wiki_directory: &Path, wiki_path: &Path) -> Vec<String> {
    // Track valid targets while visiting nodes and links in deterministic order.
    let mut referenced_files = HashSet::<PathBuf>::new();
    let mut referenced_directories = HashSet::<PathBuf>::new();
    let mut errors = Vec::<String>::new();
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| &node.title);
    'nodes: for node in nodes {
        let mut links = node.links.iter().collect::<Vec<_>>();
        links.sort();
        for link in links {
            // Skip text links before matching on the link again below [tag:skip_text_links].
            let path = match link {
                Link::Text(_) => continue,
                Link::File(path) | Link::Directory(path) => path,
            };

            // Follow symbolic links when classifying each target.
            let target = wiki_directory.join(path);
            let metadata = match fs::metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) => {
                    errors.push(format!(
                        "Node {} links to inaccessible path {}: {error}",
                        node.title.code_str(),
                        path.code_path(),
                    ));
                    if errors.len() >= MAX_FILESYSTEM_ERRORS {
                        break 'nodes;
                    }
                    continue;
                }
            };

            // Retain correctly typed targets and report links with the wrong type.
            match link {
                Link::File(_) if metadata.is_file() => {
                    referenced_files.insert(target);
                }
                Link::Directory(_) if metadata.is_dir() => {
                    referenced_directories.insert(target);
                }
                Link::File(_) => errors.push(format!(
                    "Node {} links to {}, which is not a file.",
                    node.title.code_str(),
                    path.code_path(),
                )),
                Link::Directory(_) => errors.push(format!(
                    "Node {} links to {}, which is not a directory.",
                    node.title.code_str(),
                    path.code_path(),
                )),
                Link::Text(_) => {
                    // Text links were skipped above [ref:skip_text_links].
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
        return errors;
    }

    // Spend the remaining error budget on uncovered filesystem entries.
    let remaining_error_capacity = MAX_FILESYSTEM_ERRORS - errors.len();
    errors.extend(find_unreferenced_filesystem_links(
        wiki_directory,
        wiki_path,
        &referenced_files,
        &referenced_directories,
        remaining_error_capacity,
    ));

    errors
}

// Find unreferenced files within a budget while pruning covered directories.
fn find_unreferenced_filesystem_links(
    wiki_directory: &Path,
    wiki_path: &Path,
    referenced_files: &HashSet<PathBuf>,
    referenced_directories: &HashSet<PathBuf>,
    maximum_errors: usize,
) -> Vec<String> {
    // Handle a link to the wiki directory because the walk root bypasses the entry filter.
    if referenced_directories.contains(wiki_directory) {
        return Vec::new();
    }

    // Include hidden entries while retaining ignore-file behavior and excluding VCS metadata.
    let mut overrides = OverrideBuilder::new(wiki_directory);
    overrides
        .add("!.git/")
        .expect("the static .git override should be valid")
        .add("!.hg/")
        .expect("the static .hg override should be valid");
    let overrides = match overrides.build() {
        Ok(overrides) => overrides,
        Err(error) => return vec![format!("Failed to build filesystem ignore rules: {error}")],
    };

    // Follow directory symlinks while pruning subtrees covered by explicit directory links.
    let mut walker_builder = WalkBuilder::new(wiki_directory);
    walker_builder
        .current_dir(wiki_directory)
        .follow_links(true)
        .hidden(false)
        .parents(false)
        .require_git(false)
        .overrides(overrides)
        .filter_entry({
            let wiki_path = wiki_path.to_owned();
            let referenced_directories = referenced_directories.clone();
            move |entry| {
                // Exclude the wiki and prune directories already covered by their links.
                entry.path() != wiki_path && !referenced_directories.contains(entry.path())
            }
        });

    // Stop traversing once the remaining error budget is exhausted.
    let mut errors = Vec::<String>::new();
    for result in walker_builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("Failed to walk wiki directory: {error}"));
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
            errors.push(format!(
                "File {} is not referenced.",
                relative_path(wiki_directory, path).code_path(),
            ));
            if errors.len() >= maximum_errors {
                break;
            }
        }
    }

    // Present the collected walk errors in deterministic order.
    errors.sort();
    errors
}

// Convert collected validation errors into the public result type.
fn errors_to_result(errors: Errors) -> Result<(), Errors> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_FILESYSTEM_ERRORS, validate};
    use crate::parser::parse;
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

    // Validate referenced entries and prune the recursive contents of referenced directories.
    #[test]
    fn referenced_entries() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.path().join(".secret"), "secret").unwrap();
        fs::write(directory.path().join("ignored.txt"), "ignored").unwrap();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let wiki = parse(concat!(
            "# Home\n[",
            "file:.gitignore] [",
            "file:.secret] [",
            "dir:images]",
        ))
        .unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
    }

    // Allow a wiki-directory link to cover every surrounding filesystem entry.
    #[test]
    fn wiki_directory_link() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unmanaged.txt"), "content").unwrap();
        let wiki = parse(concat!("# Home\n[", "dir:.]")).unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
    }

    // Preserve graph errors when the wiki path cannot be resolved.
    #[test]
    fn missing_wiki_path() {
        let directory = TestDirectory::new();
        let wiki_path = directory.wiki_path();
        fs::remove_file(&wiki_path).unwrap();
        let wiki = parse("# Elsewhere").unwrap();

        let errors = validate(&wiki, &wiki_path).unwrap_err();
        assert_eq!(errors[0], "Wiki does not contain a `Home` node.");
        assert!(errors[1].starts_with(&format!("Failed to resolve `{}`:", wiki_path.display())));
        assert_eq!(errors.len(), 2);
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
        assert_eq!(
            errors,
            vec![format!(
                "File `{}` is not referenced.",
                photo_path.display(),
            )],
        );
    }

    // Stop validating explicit filesystem links after reaching the diagnostic limit.
    #[test]
    fn filesystem_link_error_limit() {
        let directory = TestDirectory::new();
        let links = (0..=MAX_FILESYSTEM_ERRORS)
            .map(|index| format!(concat!("[", "file:missing-{}.txt]"), index))
            .collect::<Vec<_>>()
            .join(" ");
        let wiki = parse(&format!("# Home\n{links}")).unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS);
        assert!(
            errors
                .iter()
                .all(|error| error.starts_with("Node `Home` links to inaccessible path `missing-")),
        );
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
        let wiki = parse(concat!("# Home\n[", "file:missing.txt]")).unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), MAX_FILESYSTEM_ERRORS);
        assert!(errors[0].starts_with("Node `Home` links to inaccessible path `missing.txt`:"));
        assert!(
            errors[1..]
                .iter()
                .all(|error| error.starts_with("File `unreferenced-")),
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
        let wiki = parse(concat!(
            "# Home\n[",
            "file:notes/current.txt] [",
            "file:notes/archive/old.txt]",
        ))
        .unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
    }

    // Consider empty directories referenced because all their contents are referenced.
    #[test]
    fn empty_directories() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("empty")).unwrap();
        fs::create_dir(directory.path().join("empty/nested")).unwrap();
        let wiki = parse("# Home").unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
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
        let wiki = parse(concat!(
            "# Home\n[",
            "file:target.txt] [",
            "file:first.txt]",
        ))
        .unwrap();

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec!["File `second.txt` is not referenced.".to_owned()],
        );
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
        let wiki = parse(concat!(
            "# Home\n[",
            "dir:target] [",
            "file:alias/file.txt]",
        ))
        .unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
    }

    // Allow directory symlinks outside the wiki tree and validate their logical contents.
    #[cfg(unix)]
    #[test]
    fn external_directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let external_directory = TestDirectory::new();
        symlink(external_directory.path(), directory.path().join("external")).unwrap();
        let wiki = parse(concat!("# Home\n[", "file:external/wiki.mull]")).unwrap();

        assert_eq!(validate(&wiki, &directory.wiki_path()), Ok(()));
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
        fs::write(&target_path, concat!("# Home\n[", "file:wiki.txt]")).unwrap();
        symlink("wiki.txt", &wiki_path).unwrap();
        let wiki = parse(concat!("# Home\n[", "file:wiki.txt]")).unwrap();

        assert_eq!(validate(&wiki, &wiki_path), Ok(()));
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
                .any(|error| error.starts_with("Failed to walk wiki directory:")),
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
                .any(|error| error.starts_with("Failed to walk wiki directory:")),
        );
    }

    // Reject a filesystem link whose target has the wrong type.
    #[test]
    fn wrong_target_type() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.path().join("images")).unwrap();
        fs::write(directory.path().join("images/photo.jpg"), "photo").unwrap();
        let wiki = parse(concat!("# Home\n[", "file:images]")).unwrap();
        let photo_path = Path::new("images").join("photo.jpg");

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec![
                "Node `Home` links to `images`, which is not a file.".to_owned(),
                format!("File `{}` is not referenced.", photo_path.display()),
            ],
        );
    }

    // Reject text links that do not correspond to any node in the wiki.
    #[test]
    fn missing_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Zulu] and [Alpha].").unwrap();

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec![
                "Node `Home` links to missing node `Alpha`.".to_owned(),
                "Node `Home` links to missing node `Zulu`.".to_owned(),
            ],
        );
    }

    // Report independent graph, filesystem-link, and unreferenced-entry errors together.
    #[test]
    fn multiple_validation_errors() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("unreferenced.txt"), "content").unwrap();
        let wiki = parse(concat!(
            "# Home\nSee [Missing] and [",
            "file:missing.txt].\n",
            "# Orphan",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error == "Node `Home` links to missing node `Missing`."),
        );
        assert!(
            errors
                .iter()
                .any(|error| error == "Node `Orphan` is not reachable from `Home`."),
        );
        assert!(errors.iter().any(|error| {
            error.starts_with("Node `Home` links to inaccessible path `missing.txt`:")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error == "File `unreferenced.txt` is not referenced."),
        );
        assert_eq!(errors.len(), 4);
    }

    // Reject an empty text link because node titles cannot be empty.
    #[test]
    fn empty_text_link() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [].").unwrap();

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec!["Node `Home` links to missing node ``.".to_owned()],
        );
    }

    // Require every wiki to contain its special root node.
    #[test]
    fn missing_home() {
        let directory = TestDirectory::new();
        let wiki = parse("# Elsewhere").unwrap();

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec!["Wiki does not contain a `Home` node.".to_owned()],
        );
    }

    // Reject nodes that cannot be reached transitively from Home.
    #[test]
    fn unreachable_nodes() {
        let directory = TestDirectory::new();
        let wiki = parse("# Home\nSee [Middle].\n# Middle\n# Zulu\n# Alpha").unwrap();

        assert_eq!(
            validate(&wiki, &directory.wiki_path()).unwrap_err(),
            vec![
                "Node `Alpha` is not reachable from `Home`.".to_owned(),
                "Node `Zulu` is not reachable from `Home`.".to_owned(),
            ],
        );
    }
}
