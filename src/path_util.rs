use std::path::Path;

// This location distinguishes local wikis from new editor buffers without filesystem context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WikiLocation<'a> {
    Local {
        path: &'a Path,
        display_path: &'a Path,
    },
    Untitled,
}

impl<'a> WikiLocation<'a> {
    // Expose the physical path only when the wiki has a surrounding filesystem to validate.
    pub fn path(self) -> Option<&'a Path> {
        match self {
            Self::Local { path, .. } => Some(path),
            Self::Untitled => None,
        }
    }

    // Attach a user-facing path to terminal errors without inventing one for editor buffers.
    pub fn display_path(self) -> Option<&'a Path> {
        match self {
            Self::Local { display_path, .. } => Some(display_path),
            Self::Untitled => None,
        }
    }
}

// Express a path relative to a base directory when the path is contained within it.
pub fn relative_path<'a>(base_directory: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(base_directory).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::relative_path;
    use std::path::Path;

    // Strip the base directory from a path contained within it.
    #[test]
    fn contained_path() {
        assert_eq!(
            relative_path(Path::new("/notes"), Path::new("/notes/archive/file.txt")),
            Path::new("archive/file.txt"),
        );
    }

    // Preserve a path that is outside the base directory.
    #[test]
    fn outside_path() {
        assert_eq!(
            relative_path(Path::new("/notes"), Path::new("/other/file.txt")),
            Path::new("/other/file.txt"),
        );
    }
}
