// This module walks the file tree under a wiki's file root, where every file must be linked from
// the wiki. Everything that validates or reads the tree shares the walk, so they all agree on
// what's in it. It's worth recording how directories are interpreted, since several decisions
// follow from it.
//
// There are two ways to think of directories:
//
// 1. A directory is an entity in its own right which needs a link, and it's implicitly linked if
//    everything in it is linked.
// 2. A directory isn't an entity which can be linked to at all. A link to a directory is shorthand
//    for links to all the files under it.
//
// Under either interpretation, a directory needn't be linked if all the files in it are. That
// matters because linking a directory covers anything added to it later, so linking each file
// instead keeps the assurance that a stray file in it will be reported. It also follows that an
// empty directory needs no link: under the first interpretation it's implicitly linked, since
// everything in it is linked, and under the second it isn't anything which could be linked.
//
// Mull takes the second interpretation for a practical reason. The first would allow a link to an
// empty directory, but Git has no way to represent one, so the link would break in a fresh clone.
// Under the second, a link to a directory without any files, not counting ignored ones, is an
// error, since a link has to match something, and the directory itself isn't anything.
//
// Empty directories themselves aren't reported. Neither interpretation calls for it, and an empty
// directory has no content that the wiki could fail to account for, so reporting it would only be
// a rule about tidiness.

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
    // The walk reaches the file, or a file within the directory.
    Visible,

    // The walk reaches the directory but no file within it.
    Empty,

    // The walk never reaches the path, because an ignore rule excludes it or one of its ancestors.
    Ignored,
}

// Determine whether a walk of the file tree reaches a path. The walk descends only along the
// path and into its target, stopping at the first file it finds there, so it applies every ignore
// rule without reading unrelated subtrees.
pub fn visibility(
    file_root: &FileRoot,
    target: &SpelledPath,
    cancellation: &CancellationFlag,
) -> Outcome<Visibility> {
    // Keep only the ancestors of the target and the entries within it.
    let mut walker_builder = file_tree_walker(file_root.path());
    walker_builder.filter_entry({
        let file_root = file_root.clone();
        let target = target.clone();
        move |entry| {
            let path = file_root.entry_path(entry);
            target.starts_with(&path) || path.starts_with(&target)
        }
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
        let path = file_root.entry_path(&entry);
        if path == *target {
            reached = true;
        }
        if path.starts_with(target)
            && entry
                .file_type()
                .expect("Only standard input lacks a file type.")
                .is_file()
        {
            return Outcome::Completed(Visibility::Visible);
        }
    }

    // Distinguish a directory without files from a path that the walk never reached.
    Outcome::Completed(if reached {
        Visibility::Empty
    } else {
        Visibility::Ignored
    })
}
