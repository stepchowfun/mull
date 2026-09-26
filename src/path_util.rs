use std::path::Path;

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

    // Preserve a path that's outside the base directory.
    #[test]
    fn outside_path() {
        assert_eq!(
            relative_path(Path::new("/notes"), Path::new("/other/file.txt")),
            Path::new("/other/file.txt"),
        );
    }
}
