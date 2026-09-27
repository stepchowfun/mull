use crate::{format::CodePath, parser::LinkPath, path_util::relative_path};
use colored::ColoredString;
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

// A path relative to the wiki directory whose components are spelled exactly as the names in their
// directories' listings. Comparing such paths as written then agrees with the filesystem, whether
// or not it ignores case. The exceptions, kept as written, are a component within a directory that
// can't be listed and the final name of a rename's destination, which the rename checks itself.
// Such a path describes the disk when it was spelled, so it shouldn't outlive a check or request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiskPath(PathBuf);

impl DiskPath {
    // Expose the path for display and for filesystem operations.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    // Determine whether this path is the wiki directory itself.
    pub fn is_wiki_directory(&self) -> bool {
        self.0.as_os_str().is_empty()
    }

    // Determine whether this path is another or lies within it.
    pub fn starts_with(&self, other: &DiskPath) -> bool {
        self.0.starts_with(&other.0)
    }

    // Find the directory containing this path, which is spelled as on disk as well.
    pub fn parent(&self) -> Option<DiskPath> {
        self.0.parent().map(|parent| DiskPath(parent.to_owned()))
    }

    // Expose the final component's name.
    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }
}

// Format a spelled path for human-facing diagnostic output.
impl CodePath for DiskPath {
    fn code_path(&self) -> ColoredString {
        self.0.code_path()
    }
}

// This is a misspelled path, along with its spelling on disk if every component has one.
#[derive(Debug)]
pub struct Misspelling {
    pub message: String,
    pub spelled: Option<DiskPath>,
}

// This is the directory containing a wiki, as given, with the wiki's path within it spelled as on
// disk. An editor or a user may spell the wiki's path differently if the filesystem ignores case.
// The directory's own spelling doesn't matter, since it's only a prefix from which every other path
// is derived, and it's never compared with a path spelled independently of it.
#[derive(Clone, Debug)]
pub struct WikiDirectory {
    path: PathBuf,
    wiki_path: DiskPath,
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
            match check_spelling(&path, relative_wiki_path, &mut DirectoryListings::new()) {
                Ok(spelled) => DiskPath(spelled),
                Err(Misspelling {
                    spelled: Some(spelled),
                    ..
                }) => spelled,
                Err(Misspelling { spelled: None, .. }) => DiskPath(relative_wiki_path.to_owned()),
            };
        Self { path, wiki_path }
    }

    // Expose the directory as given, which filesystem operations resolve paths against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Expose the wiki's path within the directory, spelled as on disk.
    pub fn wiki_path(&self) -> &DiskPath {
        &self.wiki_path
    }

    // Locate a spelled path for filesystem operations.
    pub fn resolve(&self, path: &DiskPath) -> PathBuf {
        self.path.join(&path.0)
    }

    // Spell the path of an entry found by walking the wiki directory, whose components come from
    // directory listings.
    pub fn entry_path(&self, entry: &ignore::DirEntry) -> DiskPath {
        DiskPath(
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
        path: &LinkPath,
        listings: &mut DirectoryListings,
    ) -> Result<DiskPath, Misspelling> {
        check_spelling(&self.path, path.as_path(), listings).map(DiskPath)
    }

    // Require the existing directories along a rename's new path to be spelled as on disk. The
    // other names don't exist, so they have no other spelling. The final name is kept as written
    // even if it exists: the rename refuses such a destination, other than within a directory
    // moving into itself, where nothing will exist at the time of the move.
    pub fn spell_destination(&self, path: &LinkPath) -> Result<DiskPath, Misspelling> {
        let path = path.as_path();
        let Some(ancestor) = path
            .ancestors()
            .skip(1)
            .find(|ancestor| self.path.join(ancestor).exists())
        else {
            return Ok(DiskPath(path.to_owned()));
        };
        let spelled = check_spelling(&self.path, ancestor, &mut DirectoryListings::new())?;
        Ok(DiskPath(spelled.join(path.strip_prefix(ancestor).expect(
            "An ancestor of a path should be a prefix of it.",
        ))))
    }
}

// These are the names of the entries in each directory, listed at most once per validation or
// rename. A directory that can't be listed has no names.
pub type DirectoryListings = HashMap<PathBuf, Option<HashSet<OsString>>>;

// Compare each component of an existing target's path with the names of the entries on disk.
// Filesystems that ignore case or Unicode normalization find a target even when its path is spelled
// differently, but such a link would break on other filesystems and wouldn't match the names found
// when walking the wiki's directory. Return the path as spelled on disk, or describe the
// misspelling along with the spelling on disk if every component has one.
fn check_spelling(
    wiki_directory: &Path,
    path: &Path,
    listings: &mut DirectoryListings,
) -> Result<PathBuf, Misspelling> {
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
    match unmatched {
        Some(unmatched) => Err(Misspelling {
            message: format!(
                "{} doesn't match the spelling of any name on disk.",
                unmatched.code_path(),
            ),
            spelled: None,
        }),
        None if misspelled => Err(Misspelling {
            message: format!(
                "{} is spelled {} on disk.",
                path.code_path(),
                spelled.code_path(),
            ),
            spelled: Some(DiskPath(spelled)),
        }),
        None => Ok(spelled),
    }
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
