use crate::{format::CodePath, path_util::relative_path, wiki::FilesystemTarget};
use colored::ColoredString;
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
};

// A path relative to the wiki directory whose components are spelled exactly as the names in their
// directories' listings. Comparing such paths as written then agrees with the filesystem, whether
// or not it ignores case. The exception, kept as written, is the final name of a rename's
// destination, which the rename checks itself. Such a path describes the disk when it was spelled,
// so it shouldn't outlive a check or request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SpelledPath(PathBuf);

impl SpelledPath {
    // Expose the path for display and for filesystem operations.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    // Determine whether this path is the wiki directory itself.
    pub fn is_wiki_directory(&self) -> bool {
        self.0.as_os_str().is_empty()
    }

    // Determine whether this path is another or lies within it.
    pub fn starts_with(&self, other: &SpelledPath) -> bool {
        self.0.starts_with(&other.0)
    }

    // Find the directory containing this path, which is spelled as on disk as well.
    pub fn parent(&self) -> Option<SpelledPath> {
        self.0.parent().map(|parent| SpelledPath(parent.to_owned()))
    }

    // Expose the final component's name.
    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    // Spell a path, relative to a directory, as it is on disk, by comparing each component with the
    // names in its parent directory's listing. Filesystems that ignore case or Unicode
    // normalization find an entry even when its path is spelled differently, but such a path would
    // break on other filesystems and wouldn't match the names found when walking the directory.
    // Explain why the spelling isn't confirmed: either the path is misspelled, along with its
    // spelling on disk if every component has one, or a directory along it can't be listed.
    fn spell(
        wiki_directory: &Path,
        path: &Path,
        listings: &mut DirectoryListings,
    ) -> Result<Self, SpellingError> {
        let mut written = PathBuf::new();
        let mut spelled = PathBuf::new();
        let mut misspelled = false;
        for component in path.components() {
            // Accept a name that its directory lists. If the directory can't be listed, the
            // spelling can't be checked at all.
            let name = component.as_os_str();
            written.push(name);
            let directory = wiki_directory.join(&spelled);
            let names = match listings.entry(directory.clone()).or_insert_with(|| {
                fs::read_dir(&directory)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|entry| fs::DirEntry::file_name(&entry))
                            .collect()
                    })
                    .map_err(Rc::new)
            }) {
                Ok(names) => names,
                Err(error) => {
                    return Err(SpellingError {
                        message: format!(
                            "Unable to list {}, so the spelling of {} can't be checked.",
                            if spelled.as_os_str().is_empty() {
                                "the wiki directory".to_owned()
                            } else {
                                spelled.code_path().to_string()
                            },
                            path.code_path(),
                        ),
                        spelled: None,
                        reason: Some(error.clone()),
                    });
                }
            };
            if names.contains(name) {
                spelled.push(name);
                continue;
            }

            // Find the entry that the name refers to, preferring the first in sorted order if
            // several entries are indistinguishable.
            let identity = entry_identity(&directory.join(name));
            let mut candidates = names
                .iter()
                .filter(|candidate| {
                    identity.is_some() && entry_identity(&directory.join(candidate)) == identity
                })
                .collect::<Vec<_>>();
            candidates.sort();

            // Continue with the name on disk, or stop at a name that has none, since nothing
            // beneath it can be checked.
            let Some(actual) = candidates.first() else {
                return Err(SpellingError {
                    message: format!(
                        "{} doesn't match the spelling of any name on disk.",
                        written.code_path(),
                    ),
                    spelled: None,
                    reason: None,
                });
            };
            misspelled = true;
            spelled.push(actual);
        }

        // Describe the whole path's spelling on disk if it differs from how it's written.
        if misspelled {
            Err(SpellingError {
                message: format!(
                    "{} is spelled {} on disk.",
                    path.code_path(),
                    spelled.code_path(),
                ),
                spelled: Some(SpelledPath(spelled)),
                reason: None,
            })
        } else {
            Ok(SpelledPath(spelled))
        }
    }
}

// Format a spelled path for human-facing diagnostic output.
impl CodePath for SpelledPath {
    fn code_path(&self) -> ColoredString {
        self.0.code_path()
    }
}

// This explains why a path's spelling isn't confirmed: it's misspelled, along with its spelling on
// disk if every component has one, or a directory along it can't be listed, for an underlying
// reason.
#[derive(Debug)]
pub struct SpellingError {
    pub message: String,
    pub spelled: Option<SpelledPath>,
    pub reason: Option<Rc<io::Error>>,
}

// This is the directory containing a wiki, as given, with the wiki's path within it spelled as on
// disk. An editor or a user may spell the wiki's path differently if the filesystem ignores case.
// The directory's own spelling doesn't matter, since it's only a prefix from which every other path
// is derived, and it's never compared with a path spelled independently of it.
#[derive(Clone, Debug)]
pub struct WikiDirectory {
    path: PathBuf,
    wiki_path: SpelledPath,
}

impl WikiDirectory {
    // Find the directory containing a wiki, and spell the wiki's path within it.
    pub fn new(wiki_path: &Path) -> Self {
        let path = wiki_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_owned();

        // Keep the path as given for a wiki that isn't on disk, which has no other spelling.
        let relative_wiki_path = relative_path(&path, wiki_path);
        let wiki_path =
            match SpelledPath::spell(&path, relative_wiki_path, &mut DirectoryListings::new()) {
                Ok(spelled)
                | Err(SpellingError {
                    spelled: Some(spelled),
                    ..
                }) => spelled,
                Err(SpellingError { spelled: None, .. }) => {
                    SpelledPath(relative_wiki_path.to_owned())
                }
            };
        Self { path, wiki_path }
    }

    // Expose the directory as given, which filesystem operations resolve paths against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Expose the wiki's path within the directory, spelled as on disk.
    pub fn wiki_path(&self) -> &SpelledPath {
        &self.wiki_path
    }

    // Locate a spelled path for filesystem operations.
    pub fn resolve(&self, path: &SpelledPath) -> PathBuf {
        self.path.join(&path.0)
    }

    // Spell the path of an entry found by walking the wiki directory, whose components come from
    // directory listings.
    pub fn entry_path(&self, entry: &ignore::DirEntry) -> SpelledPath {
        SpelledPath(
            entry
                .path()
                .strip_prefix(&self.path)
                .expect("A walk of the wiki directory should only find entries within it.")
                .to_owned(),
        )
    }

    // Require each component of an existing entry's path to be spelled as on disk.
    pub fn spell(
        &self,
        target: &FilesystemTarget,
        listings: &mut DirectoryListings,
    ) -> Result<SpelledPath, SpellingError> {
        SpelledPath::spell(&self.path, target.path(), listings)
    }

    // Require the existing directories along a rename's new path to be spelled as on disk. The
    // other names don't exist, so they have no other spelling. The final name is kept as written
    // even if it exists: the rename refuses such a destination, other than within a directory
    // moving into itself, where nothing will exist at the time of the move.
    pub fn spell_destination(
        &self,
        target: &FilesystemTarget,
    ) -> Result<SpelledPath, SpellingError> {
        let path = target.path();
        let Some(ancestor) = path
            .ancestors()
            .skip(1)
            .find(|ancestor| self.path.join(ancestor).exists())
        else {
            return Ok(SpelledPath(path.to_owned()));
        };
        let spelled = SpelledPath::spell(&self.path, ancestor, &mut DirectoryListings::new())?;
        Ok(SpelledPath(
            spelled.0.join(
                path.strip_prefix(ancestor)
                    .expect("An ancestor of a path should be a prefix of it."),
            ),
        ))
    }
}

// These are the names of the entries in each directory, listed at most once per validation or
// rename, or else why a directory can't be listed.
pub type DirectoryListings = HashMap<PathBuf, Result<HashSet<OsString>, Rc<io::Error>>>;

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

#[cfg(test)]
mod tests {
    use super::{DirectoryListings, WikiDirectory};
    use crate::wiki::FilesystemTarget;
    use std::{env, fs, process};

    // Report the first name that matches nothing on disk, rather than failing to list a directory
    // that doesn't exist beneath it.
    #[test]
    fn missing_component() {
        let directory = env::temp_dir().join(format!("mull-spelling-{}", process::id()));
        fs::create_dir_all(&directory).unwrap();
        let wiki_directory = WikiDirectory::new(&directory.join("wiki.mull"));

        let error = wiki_directory
            .spell(
                &FilesystemTarget::parse("/missing/file.txt").unwrap(),
                &mut DirectoryListings::new(),
            )
            .unwrap_err();
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(
            error.message,
            "`missing` doesn't match the spelling of any name on disk.",
        );
    }
}
