use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::{Error, Fix, SourceRange},
    file_tree::{Visibility, file_tree_walker, visibility},
    format::CodeStr,
    line_index::LineIndex,
    scripts::script_path,
    spelled_path::{DirectoryListings, FileRoot, SpelledPath},
    wiki::{FilesystemTarget, HOME_TITLE, Link, Page, PageTitle, Wiki},
};
use std::{collections::HashSet, fs, path::Path, sync::Arc};

// Limit diagnostics for unreferenced files and directories so pathological directories remain
// manageable.
const MAX_FILESYSTEM_ERRORS: usize = 50;

// Check text links, portals, reachability, scripts, filesystem links, and filesystem coverage.
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
        errors.extend(validate_scripts(&pages, None, source_contents, line_index));
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
    errors.extend(validate_scripts(
        &pages,
        Some((&file_root, wiki_path)),
        source_contents,
        line_index,
    ));
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

// Validate text-link targets, the absence of cycles of portals, and reachability from the home
// page, given the wiki's pages in title order. Missing sections are reported, but there's no fix
// for them, since a section's place in its page can't be chosen for it.
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
    let has_home = wiki.pages.contains_key(&PageTitle::home());
    if !has_home {
        errors.push(Error::new(
            &format!("The wiki doesn't contain a {} page.", HOME_TITLE.code_str()),
            source_path,
            None,
            None,
            Some(Fix::CreatePage(PageTitle::home())),
        ));
    }

    // Validate text-link targets.
    for page in pages {
        // Report each missing page at the corresponding text-link occurrence, which declaring the
        // page would fix, and each missing section. Filesystem links are checked separately, and
        // malformed links have no target.
        for link in &page.links {
            let Link::Text {
                target,
                source_range,
                ..
            } = link
            else {
                continue;
            };
            let (message, fix) = match (wiki.pages.get(&target.title), &target.label) {
                (None, _) => (
                    format!("Page {} not found.", target.title.code_str()),
                    Some(Fix::CreatePage(target.title.clone())),
                ),
                (Some(target_page), Some(label)) if target_page.section(label).is_none() => (
                    format!(
                        "Page {} has no section {}.",
                        target.title.code_str(),
                        label.code_str(),
                    ),
                    None,
                ),
                (Some(_), _) => continue,
            };
            errors.push(Error::new(
                &message,
                source_path,
                Some((source_contents, line_index, *source_range)),
                None,
                fix,
            ));
        }
    }

    // Reject portals which close cycles of portals, since there's no finite copy of the content of
    // the pages in a cycle.
    errors.extend(
        wiki.cycle_closing_portals(pages.iter().map(|page| page.target(None)))
            .into_iter()
            .map(|portal| {
                Error::new(
                    &format!(
                        "A portal to {} here creates a cycle of portals.",
                        portal.target.code_str(),
                    ),
                    source_path,
                    Some((source_contents, line_index, portal.link_source_range)),
                    None,
                    None,
                )
            }),
    );

    // Reject every page outside the graph rooted at the home page.
    if has_home {
        errors.extend(
            pages
                .iter()
                .filter(|page| page.traversal_index.is_none())
                .map(|page| {
                    Error::new(
                        &format!(
                            "Page {} can't be reached by following links starting from {}.",
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
            Link::Text { .. } | Link::Malformed { .. } => None,
        })
        .collect()
}

// Require each script block's name to identify exactly one script, given the wiki's pages in
// title order. Scripts can only be found in a saved wiki's file root.
fn validate_scripts(
    pages: &[&Page],
    file_root: Option<(&FileRoot, &Path)>,
    source_contents: &str,
    line_index: &LineIndex,
) -> Vec<Error> {
    pages
        .iter()
        .flat_map(|page| &page.script_blocks)
        .filter_map(|block| {
            let message = match file_root {
                Some((file_root, _wiki_path)) => script_path(file_root, &block.name).err()?,
                None => "Save the wiki to find this script.".to_owned(),
            };
            Some(Error::new(
                &message,
                file_root.map(|(_file_root, wiki_path)| wiki_path),
                Some((source_contents, line_index, block.name_source_range)),
                None,
                None,
            ))
        })
        .collect()
}

// Validate filesystem links and coverage, given the wiki's pages in title order. A script which a
// script block uses counts as linked.
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

            // Skip text and malformed links, which have no filesystem targets.
            let target = match link {
                Link::Filesystem { target, .. } => target,
                Link::Text { .. } | Link::Malformed { .. } => continue,
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

            // Require the target not to be ignored. Skip this if the path's spelling wasn't
            // confirmed, which was already reported.
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

            // Track the target so the walk for unreferenced entries accounts for it.
            if let Some(spelled) = spelled {
                if target.is_directory() {
                    referenced_directories.insert(spelled);
                } else {
                    referenced_files.insert(spelled);
                }
            }
        }
    }

    // Count each script which a script block uses as linked, spelled as it was found.
    for page in pages {
        for block in &page.script_blocks {
            if let Ok(script) = script_path(file_root, &block.name)
                && let Some(name) = script
                    .strip_prefix(file_root.path())
                    .ok()
                    .and_then(Path::to_str)
                && let Ok(target) = FilesystemTarget::from_name(name, false)
                && let Ok(spelled) = file_root.spell(&target, &mut listings)
            {
                referenced_files.insert(spelled);
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

// Explain why a walk of the file tree doesn't reach a filesystem link's target, where `spelled` is
// the target's spelling on disk. The target is known to exist, so an ignore rule must exclude it,
// unless a directory along the way can't be read.
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
            Visibility::Reached => return None,
            Visibility::Unreached => format!("{} is ignored.", target.code_str()),
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

// Find unreferenced files and directories, up to a limit so pathological directories remain
// manageable, while pruning covered directories. A directory needs a link of its own unless it
// contains a file or a linked directory, and only the outermost of those needing one is reported.
#[allow(
    clippy::too_many_lines,
    reason = "The function walks the file tree once, noting what each directory contains as it \
              reports unreferenced files."
)]
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

    // Note the directories walked, those with any entries, and those containing a file or a linked
    // directory, which the walk skips.
    let mut directories = Vec::<SpelledPath>::new();
    let mut listed_directories = HashSet::<SpelledPath>::new();
    let mut occupied_directories = HashSet::<SpelledPath>::new();
    let occupy = |path: &SpelledPath, occupied_directories: &mut HashSet<SpelledPath>| {
        let mut ancestor = path.parent();
        while let Some(directory) = ancestor
            && occupied_directories.insert(directory.clone())
        {
            ancestor = directory.parent();
        }
    };
    for directory in referenced_directories {
        occupy(directory, &mut occupied_directories);
    }

    // Stop traversing once the error limit is reached, or a directory can't be read, after which
    // which directories are empty isn't known.
    let mut errors = Vec::<Error>::new();
    let mut is_complete = true;
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
                is_complete = false;
                if errors.len() >= MAX_FILESYSTEM_ERRORS {
                    break;
                }
                continue;
            }
        };

        // Record the entry, and report a file which isn't linked.
        let path = file_root.entry_path(&entry);
        if let Some(parent) = path.parent() {
            listed_directories.insert(parent);
        }
        if entry
            .file_type()
            .expect("Only standard input lacks a file type.")
            .is_dir()
        {
            directories.push(path);
            continue;
        }
        occupy(&path, &mut occupied_directories);
        if !referenced_files.contains(&path) {
            errors.push(Error::new(
                &format!("File {} isn't linked to.", path.code_str()),
                Some(wiki_path),
                None,
                None,
                None,
            ));
            if errors.len() >= MAX_FILESYSTEM_ERRORS {
                is_complete = false;
                break;
            }
        }
    }

    // Report the outermost directories which contain nothing linked, once the whole tree has been
    // walked. Linking everything in a directory covers it, unless there's nothing in it.
    if is_complete {
        for directory in directories {
            if occupied_directories.contains(&directory)
                || directory
                    .parent()
                    .is_some_and(|parent| !occupied_directories.contains(&parent))
            {
                continue;
            }
            let fix = if listed_directories.contains(&directory) {
                "Link to it or to everything in it, or delete it."
            } else {
                "Link to it, or delete it."
            };
            errors.push(Error::new(
                &format!("Directory {} isn't linked to. {fix}", directory.code_str()),
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
            "Page `Alpha` can't be reached by following links starting from `Home`.",
        ));
        assert!(contains_error(
            &errors,
            "Page `Zulu` can't be reached by following links starting from `Home`.",
        ));
    }

    // Reject a portal which closes each cycle of portals, including a portal to the page it's in,
    // at the link in its opening fence. The search for cycles starts from pages in title order.
    // Pages shown through several paths without a cycle are fine.
    #[test]
    fn portal_cycles() {
        let directory = TestDirectory::new();
        let wiki = parse(concat!(
            "# Home\n[Bottom] [Last] [Left] [Middle] [Right] [Self] [Top]\n",
            "# Top\n::: [Left]\n:::\n::: [Right]\n:::\n",
            "# Left\n::: [Bottom]\n:::\n",
            "# Right\n::: [Bottom]\n:::\n",
            "# Bottom\n",
            "# Middle\n::: [Last]\n:::\n",
            "# Last\n::: [Middle]\n:::\n",
            "# Self\n::: [Self]\n:::\n",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        for title in ["Last", "Self"] {
            assert!(contains_error(
                &errors,
                &format!("A portal to `{title}` here creates a cycle of portals."),
            ));
        }
    }

    // Reject a link to a section which its page doesn't have, without a fix, and don't let a link
    // to a section of a page make the page reachable.
    #[test]
    fn section_links() {
        let wiki = parse(concat!(
            "# Home\n[Notes#Missing] [Notes#Present] [#Local] [#Absent]\n## Local\n",
            "# Notes\n## Present",
        ))
        .unwrap();

        let errors = validate_untitled(&wiki).unwrap_err();
        assert_eq!(errors.len(), 3);
        assert!(contains_error(
            &errors,
            "Page `Notes` has no section `Missing`.",
        ));
        assert!(contains_error(
            &errors,
            "Page `Home` has no section `Absent`.",
        ));
        assert!(contains_error(
            &errors,
            "Page `Notes` can't be reached by following links starting from `Home`.",
        ));
        assert!(errors.iter().all(|error| error.fix().is_none()));
    }

    // Don't let a portal to a page make the page reachable, since showing it doesn't justify it.
    #[test]
    fn portals_dont_reach_pages() {
        let wiki = parse("# Home\n::: [Shown]\n:::\n# Shown").unwrap();

        let errors = validate_untitled(&wiki).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "Page `Shown` can't be reached by following links starting from `Home`.",
        ));
    }

    // Reject a portal which closes a cycle through a section, where showing a section means
    // showing the portals within it, but not a portal to a section of the page it's in outside that
    // section.
    #[test]
    fn section_portal_cycles() {
        let wiki = parse(concat!(
            "# Home\n[Other]\n::: [#Notes] :::\n## Notes\n::: [Other#Left] :::\n",
            "# Other\n## Left\n::: [Home#Notes] :::\n## Right\n::: [#Left] :::\n",
        ))
        .unwrap();

        let errors = validate_untitled(&wiki).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "A portal to `Home#Notes` here creates a cycle of portals.",
        ));
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

    // Require each script block's name to identify exactly one script, whatever its extension.
    #[test]
    fn scripts() {
        let directory = TestDirectory::new();
        directory.write("scripts/sort.sh", "");
        directory.write("scripts/tidy.py", "");
        directory.write("scripts/tidy.sh", "");
        let wiki = parse(concat!(
            "# Home\n[/scripts/]\n",
            "::: sort -f\n:::\n::: tidy :::\n::: missing :::\n",
        ))
        .unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "No script named `missing` in `/scripts`.",
        ));
        assert!(contains_error(
            &errors,
            "More than one script is named `tidy` in `/scripts`: `/scripts/tidy.py`, \
             `/scripts/tidy.sh`.",
        ));
    }

    // Ask for an unsaved wiki to be saved before finding its scripts.
    #[test]
    fn untitled_scripts() {
        let wiki = parse("# Home\n::: sort :::").unwrap();

        let errors = validate_untitled(&wiki).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message(), "Save the wiki to find this script.");
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

    // Report only files in the file root, not other files beside the wiki.
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

    // Reject a file root that isn't a directory.
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

    // Allow a link to the file root to cover every file in it.
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

    // Accept a link to a directory without files, including one whose files are all ignored.
    #[test]
    fn directory_without_files() {
        let directory = TestDirectory::new();
        directory.write(".gitignore", "*.log\n");
        directory.create_dir("empty/nested");
        directory.write("logs/debug.log", "debug");
        directory.write("deep/nested/file.txt", "file");
        let wiki = parse("# Home\n[/.gitignore] [/empty/] [/logs/] [/deep/]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
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

    // Count a script which a script block uses as linked, but not one which none uses.
    #[test]
    fn used_scripts_are_linked() {
        let directory = TestDirectory::new();
        directory.write("scripts/sort.sh", "");
        directory.write("scripts/unused.sh", "");
        let wiki = parse("# Home\n::: sort :::\n").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "File `/scripts/unused.sh` isn't linked to.",
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

    // Report every broken filesystem link, since the limit applies only to unreferenced files and
    // directories.
    #[test]
    fn filesystem_link_errors_are_unlimited() {
        let directory = TestDirectory::new();
        directory.create_dir("");
        let links = (0..=MAX_FILESYSTEM_ERRORS)
            .map(|index| format!("[/missing-{index}.txt]"))
            .chain(["[/]".to_owned()])
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

    // Limit diagnostics for unreferenced files regardless of any link errors, and leave out empty
    // directories once the limit stops the walk.
    #[test]
    fn unreferenced_file_error_limit() {
        let directory = TestDirectory::new();
        for index in 0..=MAX_FILESYSTEM_ERRORS {
            directory.write(&format!("unreferenced-{index}.txt"), "");
        }
        directory.create_dir("empty");
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

    // Accept a directory without a link when all the files in it are linked.
    #[test]
    fn implicitly_referenced_directories() {
        let directory = TestDirectory::new();
        directory.write("notes/current.txt", "current");
        directory.write("notes/archive/old.txt", "old");
        let wiki = parse("# Home\n[/notes/current.txt] [/notes/archive/old.txt]").unwrap();

        assert!(validate(&wiki, &directory.wiki_path()).is_ok());
    }

    // Report each outermost directory with nothing linked in it, suggesting linking everything in
    // it only if it has anything in it. A directory containing a linked directory, or within a
    // linked directory, needs no link.
    #[test]
    fn empty_directories() {
        let directory = TestDirectory::new();
        directory.write("notes.txt", "notes");
        directory.create_dir("bare");
        directory.create_dir("empty/nested");
        directory.create_dir("outer/inner");
        directory.create_dir("linked/inside");
        let wiki = parse("# Home\n[/notes.txt] [/outer/inner/] [/linked/]").unwrap();

        let errors = validate(&wiki, &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(contains_error(
            &errors,
            "Directory `/bare` isn't linked to. Link to it, or delete it.",
        ));
        assert!(contains_error(
            &errors,
            "Directory `/empty` isn't linked to. Link to it or to everything in it, or delete it.",
        ));
    }

    // Treat the file root like any other directory, reporting it alone if nothing in it is linked.
    #[test]
    fn empty_file_root() {
        let directory = TestDirectory::new();
        directory.create_dir("");

        assert!(contains_error(
            &validate(&parse("# Home").unwrap(), &directory.wiki_path()).unwrap_err(),
            "Directory `/` isn't linked to. Link to it, or delete it.",
        ));
        assert!(validate(&parse("# Home\n[/]").unwrap(), &directory.wiki_path()).is_ok());
        directory.create_dir("empty");
        let errors = validate(&parse("# Home").unwrap(), &directory.wiki_path()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(contains_error(
            &errors,
            "Directory `/` isn't linked to. Link to it or to everything in it, or delete it.",
        ));
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
            "Page `Orphan` can't be reached by following links starting from `Home`.",
            "`/missing.txt` not found in `",
            "File `/unreferenced.txt` isn't linked to.",
        ] {
            assert!(contains_error(&errors, message));
        }
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
