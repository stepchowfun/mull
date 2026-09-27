use crate::{
    cancellation::{CancellationFlag, Outcome},
    spelled_path::{SpelledPath, WikiDirectory},
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::path::Path;

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

// Determine whether a walk of the wiki tree reaches a path. The walk descends only along the path
// and into its target, stopping at the first file it finds there, so it applies every ignore rule
// without reading unrelated subtrees.
pub fn visibility(
    wiki_directory: &WikiDirectory,
    target: &SpelledPath,
    cancellation: &CancellationFlag,
) -> Outcome<Result<Visibility, ignore::Error>> {
    // Keep only the ancestors of the target and the entries within it.
    let mut walker_builder = match wiki_tree_walker(wiki_directory.path()) {
        Ok(walker_builder) => walker_builder,
        Err(error) => return Outcome::Completed(Err(error)),
    };
    walker_builder.filter_entry({
        let wiki_directory = wiki_directory.clone();
        let target = target.clone();
        move |entry| {
            let path = wiki_directory.entry_path(entry);
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
        let path = wiki_directory.entry_path(&entry);
        if path == *target {
            reached = true;
        }
        if path.starts_with(target) && entry.file_type().is_some_and(|t| t.is_file()) {
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
