use std::{ffi::OsStr, path::Path};

// These file names, in lowercase, have languages which editors don't recognize by the names
// themselves, so a code block names the language instead. Editors recognize some other names, like
// `dockerfile`.
const FILE_NAME_LANGUAGES: &[(&str, &str)] = &[
    (".bash_profile", "shellscript"),
    (".bashrc", "shellscript"),
    (".editorconfig", "properties"),
    (".env", "dotenv"),
    (".gitattributes", "properties"),
    (".gitconfig", "properties"),
    (".gitignore", "ignore"),
    (".profile", "shellscript"),
    (".zprofile", "shellscript"),
    (".zshrc", "shellscript"),
    ("brewfile", "ruby"),
    ("gemfile", "ruby"),
    ("gnumakefile", "makefile"),
    ("jenkinsfile", "groovy"),
    ("pkgbuild", "shellscript"),
    ("podfile", "ruby"),
    ("rakefile", "ruby"),
    ("vagrantfile", "ruby"),
];

// These file extensions, in lowercase, have languages which editors don't recognize by the
// extensions themselves, so a code block names the language instead. Editors recognize most other
// extensions.
const EXTENSION_LANGUAGES: &[(&str, &str)] = &[
    ("bib", "bibtex"),
    ("cc", "cpp"),
    ("cfg", "properties"),
    ("cjs", "javascript"),
    ("clj", "clojure"),
    ("cljc", "clojure"),
    ("cljs", "clojure"),
    ("cmd", "bat"),
    ("conf", "properties"),
    ("cs", "csharp"),
    ("csx", "csharp"),
    ("cts", "typescript"),
    ("cu", "cuda-cpp"),
    ("cuh", "cuda-cpp"),
    ("cxx", "cpp"),
    ("edn", "clojure"),
    ("env", "dotenv"),
    ("erb", "ruby"),
    ("fs", "fsharp"),
    ("fsi", "fsharp"),
    ("fsx", "fsharp"),
    ("gemspec", "ruby"),
    ("gradle", "groovy"),
    ("h", "cpp"),
    ("hh", "cpp"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("jl", "julia"),
    ("m", "objective-c"),
    ("mak", "makefile"),
    ("md", "markdown"),
    ("mjs", "javascript"),
    ("mk", "makefile"),
    ("mm", "objective-cpp"),
    ("mts", "typescript"),
    ("ndjson", "jsonl"),
    ("patch", "diff"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("psd1", "powershell"),
    ("psm1", "powershell"),
    ("pyi", "python"),
    ("pyw", "python"),
    ("rake", "ruby"),
    ("rs", "rust"),
    ("rst", "restructuredtext"),
    ("vbs", "vb"),
    ("xaml", "xml"),
    ("xsd", "xml"),
    ("xslt", "xsl"),
    ("yml", "yaml"),
];

// Name the language of a file for a code block. Editors recognize many languages by their usual
// file extensions, like `js`, or by the names of files without extensions, like `Dockerfile`. The
// tables give the names of other languages, like `rust` for `rs`. Editors highlight names they
// don't recognize as plain text. A file without such a name gets plain text explicitly, since
// editors highlight a code block without a language like the document containing it.
pub fn code_block_language(path: &Path) -> String {
    // Look up the file's name, then its extension, ignoring case.
    let lowercase = |name: Option<&OsStr>| name?.to_str().map(str::to_ascii_lowercase);
    let file_name = lowercase(path.file_name());
    let extension = lowercase(path.extension());
    let look_up = |table: &[(&str, &str)], name: &Option<String>| {
        table
            .iter()
            .find(|(table_name, _)| Some(*table_name) == name.as_deref())
            .map(|(_, language)| (*language).to_owned())
    };
    if let Some(language) = look_up(FILE_NAME_LANGUAGES, &file_name)
        .or_else(|| look_up(EXTENSION_LANGUAGES, &extension))
    {
        return language;
    }

    // Otherwise, use the extension, or the file's name if it has no extension, if that can be a
    // language's name.
    let name = if path.extension().is_some() {
        extension
    } else {
        file_name
    };
    name.filter(|name| {
        !name.is_empty()
            && name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '_')
            })
    })
    .unwrap_or_else(|| "text".to_owned())
}

#[cfg(test)]
mod tests {
    use super::code_block_language;
    use std::path::Path;

    // Name a file's language by the language which a table gives for its name or extension, or
    // else by its extension or, without one, its name, ignoring case. A file without such a name is
    // plain text.
    #[test]
    fn code_block_languages() {
        for (path, language) in [
            ("Gemfile", "ruby"),
            (".bashrc", "shellscript"),
            ("main.RS", "rust"),
            ("notes.md", "markdown"),
            ("script.JS", "js"),
            ("archive.tar.gz", "gz"),
            ("program.c++", "c++"),
            ("Dockerfile", "dockerfile"),
            (".hidden", "text"),
            ("odd name", "text"),
            ("notes.", "text"),
            ("odd.a`b", "text"),
            ("odd.a b", "text"),
        ] {
            assert_eq!(code_block_language(Path::new(path)), language);
        }
    }
}
