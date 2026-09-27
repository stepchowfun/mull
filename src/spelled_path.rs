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
// or not it ignores case. Such a path describes the disk when it was spelled, so it shouldn't
// outlive a check or request.
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

    // Require a path, relative to a directory, to be spelled exactly as the names in each of its
    // directories' listings. Filesystems that ignore case or Unicode normalization find an entry
    // even when its path is spelled differently, but such a path would break on other filesystems
    // and wouldn't match the names found when walking the directory. No attempt is made to guess
    // which name a misspelling refers to.
    fn spell(
        wiki_directory: &Path,
        path: &Path,
        listings: &mut DirectoryListings,
    ) -> Result<Self, SpellingError> {
        let mut spelled = PathBuf::new();
        for component in path.components() {
            // Accept a name only if its directory lists it exactly as written. If the directory
            // can't be listed, the spelling can't be checked at all.
            let name = component.as_os_str();
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
                        reason: Some(error.clone()),
                    });
                }
            };
            spelled.push(name);
            if !names.contains(name) {
                return Err(SpellingError {
                    message: format!(
                        "{} doesn't match the spelling of any name on disk.",
                        spelled.code_path(),
                    ),
                    reason: None,
                });
            }
        }
        Ok(SpelledPath(spelled))
    }
}

// Format a spelled path for human-facing diagnostic output.
impl CodePath for SpelledPath {
    fn code_path(&self) -> ColoredString {
        self.0.code_path()
    }
}

// This explains why a path's spelling isn't confirmed: a name along it isn't listed as written, or
// a directory along it can't be listed, for an underlying reason.
#[derive(Debug)]
pub struct SpellingError {
    pub message: String,
    pub reason: Option<Rc<io::Error>>,
}

// This is the directory containing a wiki, as given, with the wiki's path within it spelled as on
// disk if the wiki is there. The directory's own spelling doesn't matter, since it's only a prefix
// from which every other path is derived, and it's never compared with a path spelled
// independently of it.
#[derive(Clone, Debug)]
pub struct WikiDirectory {
    path: PathBuf,
    wiki_path: Option<SpelledPath>,
}

impl WikiDirectory {
    // Find the directory containing a wiki, and require the wiki's path within it to be spelled as
    // on disk, unless the wiki isn't there at all, as when it was deleted while open.
    pub fn new(wiki_path: &Path) -> Result<Self, SpellingError> {
        let path = wiki_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_owned();
        let spelled_wiki_path = match SpelledPath::spell(
            &path,
            relative_path(&path, wiki_path),
            &mut DirectoryListings::new(),
        ) {
            Ok(spelled) => Some(spelled),
            Err(_) if fs::symlink_metadata(wiki_path).is_err() => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            wiki_path: spelled_wiki_path,
        })
    }

    // Expose the directory as given, which filesystem operations resolve paths against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Expose the wiki's path within the directory, spelled as on disk, if the wiki is there.
    pub fn wiki_path(&self) -> Option<&SpelledPath> {
        self.wiki_path.as_ref()
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

    // Spell the deepest proper ancestor of a target that exists, which may be the wiki directory
    // itself, and return it with the rest of the target's path as written. None of the rest exists
    // except possibly the final name, which is never spelled, so a rename can tell whether it names
    // the node being renamed. An ancestor whose existence can't be determined is an error rather
    // than a missing directory.
    pub fn spell_existing_ancestor(
        &self,
        target: &FilesystemTarget,
    ) -> Result<(SpelledPath, PathBuf), SpellingError> {
        let path = target.path();
        let mut ancestor = Path::new("");
        for candidate in path.ancestors().skip(1) {
            match self.path.join(candidate).try_exists() {
                Ok(true) => {
                    ancestor = candidate;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    return Err(SpellingError {
                        message: format!(
                            "Unable to access {}.",
                            if candidate.as_os_str().is_empty() {
                                "the wiki directory".to_owned()
                            } else {
                                candidate.code_path().to_string()
                            },
                        ),
                        reason: Some(Rc::new(error)),
                    });
                }
            }
        }
        Ok((
            SpelledPath::spell(&self.path, ancestor, &mut DirectoryListings::new())?,
            path.strip_prefix(ancestor)
                .expect("An ancestor of a path should be a prefix of it.")
                .to_owned(),
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
    use crate::wiki::{ContentText, FilesystemTarget};
    use std::{env, fs, process};

    // Report an ancestor whose existence can't be determined, rather than treating it as missing.
    // Permissions don't restrict a superuser, so skip this where the ancestor remains accessible.
    #[cfg(unix)]
    #[test]
    fn inaccessible_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let directory = env::temp_dir().join(format!("mull-access-{}", process::id()));
        let locked = directory.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let wiki_directory = WikiDirectory::new(&directory.join("wiki.mull")).unwrap();

        let result = wiki_directory.spell_existing_ancestor(
            &FilesystemTarget::parse(&ContentText::from_source("/locked/inner/file.txt")).unwrap(),
        );
        let accessible = locked.join("inner").try_exists().is_ok();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&directory).unwrap();
        if !accessible {
            let inner_path = std::path::Path::new("locked").join("inner");
            assert_eq!(
                result.unwrap_err().message,
                format!("Unable to access `{}`.", inner_path.display()),
            );
        }
    }

    // Report the first name that matches nothing on disk, rather than failing to list a directory
    // that doesn't exist beneath it.
    #[test]
    fn missing_component() {
        let directory = env::temp_dir().join(format!("mull-spelling-{}", process::id()));
        fs::create_dir_all(&directory).unwrap();
        let wiki_directory = WikiDirectory::new(&directory.join("wiki.mull")).unwrap();

        let error = wiki_directory
            .spell(
                &FilesystemTarget::parse(&ContentText::from_source("/missing/file.txt")).unwrap(),
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
