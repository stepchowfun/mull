use crate::cancellation::{CancellationFlag, Outcome};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::{
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

    // Stop at the first file at or within the target.
    let mut reached = false;
    for result in walker_builder.build() {
        // Stop between entries so a superseded check doesn't walk the rest of the target.
        if cancellation.is_cancelled() {
            return Outcome::Cancelled;
        }

        // Skip failures at entries which the filter would have excluded, since the filter never
        // sees walk errors, and report any other failure.
        let entry = match result {
            Ok(entry) => entry,
            Err(error)
                if walk_error_path(&error).is_some_and(|path| {
                    !target.starts_with(path) && !path.starts_with(&target)
                }) =>
            {
                continue;
            }
            Err(error) => return Outcome::Completed(Err(error)),
        };
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

// Find the path of the entry at which a walk of the wiki tree failed.
fn walk_error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        _ => None,
    }
}

// Find a broken symlink at which a walk of the wiki tree failed, returning its path along with its
// contents. Following a chain of symlinks may reveal that a later one is the broken link, but this
// one leads nowhere all the same.
pub fn broken_symlink(error: &ignore::Error) -> Option<(&Path, PathBuf)> {
    let path = walk_error_path(error)?;
    let is_broken = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink())
        && fs::metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    if is_broken {
        Some((path, fs::read_link(path).ok()?))
    } else {
        None
    }
}
