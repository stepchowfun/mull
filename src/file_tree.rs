// This module walks the file tree under a wiki's file root, where everything must be linked from
// the wiki. Everything that validates or reads the tree shares the walk, so they all agree on
// what's in it. It's worth recording how directories are interpreted, since several decisions
// follow from it.
//
// There are two ways to think of directories: as entities in their own right, which links can name,
// or as nothing but the paths of the files under them, so a link to a directory is shorthand for
// links to those files. Mull takes the first view, since it lets an empty directory be kept
// deliberately, by linking to it, while an empty directory which isn't linked can be reported as
// clutter.
//
// A directory needs a link unless something in it is linked, whether a file or another directory.
// Linking a directory covers everything in it, including anything added to it later, so linking the
// things in it instead keeps the assurance that a stray file added to it will be reported. An empty
// directory has nothing in it to link, so it needs a link of its own. Only the outermost directory
// which needs a link is reported, since linking it covers the directories in it. The file root is
// no exception: it needs the link `[/]` if nothing in it is linked.
//
// The cost of this view is that Git has no way to represent an empty directory, so in a fresh clone
// of a wiki kept in Git, an empty directory is missing, and a link to it is broken.

use crate::{
    cancellation::{CancellationFlag, Outcome},
    spelled_path::{FileRoot, SpelledPath},
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::path::Path;

// Configure a walk of the file tree which follows directory symlinks, includes hidden
// entries, and honors ignore files within the tree while excluding VCS metadata.
pub fn file_tree_walker(file_root: &Path) -> WalkBuilder {
    // Exclude VCS metadata, which never needs links.
    let mut overrides = OverrideBuilder::new(file_root);
    overrides
        .add("!.git/")
        .expect("The static .git override should be valid.")
        .add("!.hg/")
        .expect("The static .hg override should be valid.");
    let overrides = overrides
        .build()
        .expect("The static overrides should compile.");

    // Consult ignore files only within the file tree, whether or not it's a Git repository.
    let mut walker_builder = WalkBuilder::new(file_root);
    walker_builder
        .current_dir(file_root)
        .follow_links(true)
        .hidden(false)
        .parents(false)
        .require_git(false)
        .overrides(overrides);
    walker_builder
}

// This describes whether a walk of the file tree reaches a path.
#[derive(Debug, Eq, PartialEq)]
pub enum Visibility {
    // The walk reaches the path.
    Reached,

    // The walk never reaches the path, because it doesn't exist, an ignore rule excludes it or one
    // of its ancestors, or a directory along the way can't be read.
    Unreached,
}

// Determine whether a walk of the file tree reaches a path. The walk descends only along the path,
// so it applies every ignore rule without reading unrelated subtrees.
pub fn visibility(
    file_root: &FileRoot,
    target: &SpelledPath,
    cancellation: &CancellationFlag,
) -> Outcome<Visibility> {
    // Keep only the target and its ancestors.
    let mut walker_builder = file_tree_walker(file_root.path());
    walker_builder.filter_entry({
        let file_root = file_root.clone();
        let target = target.clone();
        move |entry| target.starts_with(&file_root.entry_path(entry))
    });

    // Disregard walk failures, such as broken symlinks, since the filter never sees them, so they
    // may concern unrelated entries.
    for entry in walker_builder.build().flatten() {
        // Stop between entries so a superseded check doesn't keep walking.
        if cancellation.is_cancelled() {
            return Outcome::Cancelled;
        }

        // Stop once the walk reaches the target.
        if file_root.entry_path(&entry) == *target {
            return Outcome::Completed(Visibility::Reached);
        }
    }
    Outcome::Completed(Visibility::Unreached)
}
