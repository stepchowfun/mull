use colored::{ColoredString, Colorize, control::SHOULD_COLORIZE};
use std::path::Path;

// This trait has a function for formatting "code-like" text, such as a file path. The reason it's
// implemented as a trait and not just a function is so we can use it with method syntax, as in
// `x.code_str()`. Rust doesn't allow us to implement methods on primitive types such as `str`.
pub trait CodeStr {
    fn code_str(&self) -> ColoredString;
}

impl CodeStr for str {
    fn code_str(&self) -> ColoredString {
        // If colored output is enabled, format nonempty text in magenta. Otherwise, surround it in
        // backticks so even an empty string remains visible.
        if !self.is_empty() && SHOULD_COLORIZE.should_colorize() {
            self.magenta()
        } else {
            ColoredString::from(&format!("`{self}`") as &Self)
        }
    }
}

// Format a path as given, such as one relative to the current directory.
impl CodeStr for Path {
    fn code_str(&self) -> ColoredString {
        self.to_string_lossy().code_str()
    }
}

// Format a path relative to the wiki directory as a link would write it, starting with `/` and
// separating components with `/`, so it isn't mistaken for a path relative to the current
// directory.
pub fn code_wiki_path(path: &Path) -> ColoredString {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    format!("/{}", components.join("/")).code_str()
}

#[cfg(test)]
mod tests {
    use crate::format::{CodeStr, code_wiki_path};
    use std::path::Path;

    #[test]
    fn code_str_display() {
        // This test, like many others, depends on colors being disabled [ref:colorless_tests].
        assert_eq!(format!("{}", "foo".code_str()), "`foo`");
    }

    // Root a path relative to the wiki directory at `/`, as a link would write it.
    #[test]
    fn code_wiki_path_display() {
        assert_eq!(format!("{}", code_wiki_path(Path::new(""))), "`/`");
        assert_eq!(
            format!("{}", code_wiki_path(&Path::new("photos").join("paris"))),
            "`/photos/paris`",
        );
    }
}
