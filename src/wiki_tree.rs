use crate::{
    cancellation::{CancellationFlag, Outcome},
    format::CodePath,
    path_util::relative_path,
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

// Configure a walk of the wiki tree which follows directory symlinks, includes hidden entries, and
// honors ignore files within the tree while excluding VCS metadata.
pub fn wiki_tree_walker(wiki_directory: &Path) -> Result<WalkBuilder, ignore::Error> {
    // Exclude VCS metadata, which never needs links.
    let mut overrides = OverrideBuilder::new(wiki_directory);
    overrides
        .add("!.git/")
        .expect("The static .git override should be valid.")
        .add("!.hg/")
        .expect("The static .hg override should be valid.");
    let overrides = overrides.build()?;

    // Consult ignore files only within the wiki tree, whether or not it's a Git repository.
    let mut walker_builder = WalkBuilder::new(wiki_directory);
    walker_builder
        .current_dir(wiki_directory)
        .follow_links(true)
        .hidden(false)
        .parents(false)
        .require_git(false)
        .overrides(overrides);
    Ok(walker_builder)
}

// This describes whether a walk of the wiki tree reaches a path.
#[derive(Debug, Eq, PartialEq)]
pub enum Visibility {
    // The walk reaches the file, or a file within the directory.
    Visible,

    // The walk reaches the directory but no file within it.
    Empty,

    // The walk never reaches the path, because an ignore rule excludes it or one of its ancestors.
    Ignored,
}

// Determine whether a walk of the wiki tree reaches a path which is relative to the wiki directory.
// The walk descends only along the path and into its target, stopping at the first file it finds
// there, so it applies every ignore rule without reading unrelated subtrees.
pub fn visibility(
    wiki_directory: &Path,
    path: &Path,
    cancellation: &CancellationFlag,
) -> Outcome<Result<Visibility, ignore::Error>> {
    // Keep only the ancestors of the target and the entries within it.
    let target = wiki_directory.join(path);
    let mut walker_builder = match wiki_tree_walker(wiki_directory) {
        Ok(walker_builder) => walker_builder,
        Err(error) => return Outcome::Completed(Err(error)),
    };
    walker_builder.filter_entry({
        let target = target.clone();
        move |entry| target.starts_with(entry.path()) || entry.path().starts_with(&target)
    });

    // Disregard walk failures, such as broken symlinks, since the filter never sees them, so they
    // may concern unrelated entries.
    let mut reached = false;
    for entry in walker_builder.build().flatten() {
        // Stop between entries so a superseded check doesn't walk the rest of the target.
        if cancellation.is_cancelled() {
            return Outcome::Cancelled;
        }

        // Note reaching the target, and stop at the first file at or within it.
        if entry.path() == target {
            reached = true;
        }
        if entry.path().starts_with(&target) && entry.file_type().is_some_and(|t| t.is_file()) {
            return Outcome::Completed(Ok(Visibility::Visible));
        }
    }

    // Distinguish a directory without files from a path that the walk never reached.
    Outcome::Completed(Ok(if reached {
        Visibility::Empty
    } else {
        Visibility::Ignored
    }))
}

// Find the wiki's path relative to its directory, spelled as it is on disk. An editor or a user may
// spell the wiki's path differently on a filesystem that ignores case, but it must match the
// spellings found when walking the wiki's directory and required of links.
pub fn relative_wiki_path(wiki_directory: &Path, wiki_path: &Path) -> PathBuf {
    check_spelling(
        wiki_directory,
        relative_path(wiki_directory, wiki_path),
        &mut DirectoryListings::new(),
    )
    .0
}

// These are the names of the entries in each directory, listed at most once per validation or
// rename. A directory that can't be listed has no names.
pub type DirectoryListings = HashMap<PathBuf, Option<HashSet<OsString>>>;

// Compare each component of an existing target's path with the names of the entries on disk.
// Filesystems that ignore case or Unicode normalization find a target even when its path is spelled
// differently, but such a link would break on other filesystems and wouldn't match the names found
// when walking the wiki's directory. Return the path as spelled on disk, where that can be
// determined, with a message describing any misspelling.
pub fn check_spelling(
    wiki_directory: &Path,
    path: &Path,
    listings: &mut DirectoryListings,
) -> (PathBuf, Option<String>) {
    let mut written = PathBuf::new();
    let mut spelled = PathBuf::new();
    let mut misspelled = false;
    let mut unmatched = None;
    for component in path.components() {
        // Accept a name that its directory lists, or any name in a directory that can't be listed.
        let name = component.as_os_str();
        written.push(name);
        let directory = wiki_directory.join(&spelled);
        let names = listings.entry(directory.clone()).or_insert_with(|| {
            fs::read_dir(&directory).ok().map(|entries| {
                entries
                    .flatten()
                    .map(|entry| fs::DirEntry::file_name(&entry))
                    .collect()
            })
        });
        let Some(names) = names else {
            spelled.push(name);
            continue;
        };
        if names.contains(name) {
            spelled.push(name);
            continue;
        }

        // Find the entry that the name refers to, preferring the first in sorted order if several
        // entries are indistinguishable.
        let identity = entry_identity(&directory.join(name));
        let mut candidates = names
            .iter()
            .filter(|candidate| {
                identity.is_some() && entry_identity(&directory.join(candidate)) == identity
            })
            .collect::<Vec<_>>();
        candidates.sort();
        let actual = candidates.first().map(|candidate| (*candidate).clone());

        // Continue with the name on disk, remembering the first name that has none.
        misspelled = true;
        if actual.is_none() && unmatched.is_none() {
            unmatched = Some(written.clone());
        }
        spelled.push(actual.as_deref().unwrap_or(name));
    }

    // Describe the whole path's spelling on disk, or else the first name that has no match.
    let message = match unmatched {
        Some(unmatched) => Some(format!(
            "{} doesn't match the spelling of any name on disk.",
            unmatched.code_path(),
        )),
        None => misspelled.then(|| {
            format!(
                "{} is spelled {} on disk.",
                path.code_path(),
                spelled.code_path(),
            )
        }),
    };
    (spelled, message)
}

// Identify the directory entry that a path names without following a symlink in its last component,
// so symlinks to the same target remain distinct.
#[cfg(unix)]
pub fn entry_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

// Identify the directory entry that a path names by its resolved target where entry identities
// aren't available, which conflates symlinks to the same target.
#[cfg(not(unix))]
pub fn entry_identity(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}
