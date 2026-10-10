use crate::{error::SourceRange, format::CodeStr, spelled_path::code_file_path};
use colored::ColoredString;
use std::{
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
};

// These strings define the wiki format's extension, the suffix that names a wiki's files
// directory, and structural markers. A link whose target starts with `/` is a filesystem link,
// relative to the files directory, which names a directory if it ends with `/`.
pub const WIKI_EXTENSION: &str = "mull";
pub const FILES_DIRECTORY_SUFFIX: &str = "_files";
const TITLE_MARKER: &str = "#";
pub const TITLE_PREFIX: &str = "# ";
pub const FILESYSTEM_LINK_PREFIX: &str = "/";

// Find the text after a title line's marker, which is a title marker followed by either a space or
// the end of the line. A line that isn't a title line has none.
pub fn title_line_text(line: &str) -> Option<&str> {
    if line == TITLE_MARKER {
        Some("")
    } else {
        line.strip_prefix(TITLE_PREFIX)
    }
}
pub const DIRECTORY_LINK_SUFFIX: &str = "/";

// This title identifies the root of every wiki's text-link graph.
pub const HOME_TITLE: &str = "Home";

// This struct represents a parsed wiki.
#[derive(Clone, Debug, Default)]
pub struct Wiki {
    pub pages: HashMap<String, Page>,
}

impl Wiki {
    // Iterate over the links of every page, in no particular order.
    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.pages.values().flat_map(|page| &page.links)
    }
}

// Render pages deterministically in traversal order, with unreachable pages last in title order.
impl fmt::Display for Wiki {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort reachable pages by traversal order, followed by unreachable pages in title order.
        let mut pages = self.pages.iter().collect::<Vec<_>>();
        pages.sort_by_key(|(title, page)| rendering_order_key(page.traversal_index, title));

        // Add one line break between pages because each page already ends with one.
        for (index, (_title, page)) in pages.into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            write!(formatter, "{page}")?;
        }

        // Rendering succeeded.
        Ok(())
    }
}

// Order pages as the wiki is rendered: reachable pages by their traversal index, followed by
// unreachable pages in title order.
pub fn rendering_order_key(
    traversal_index: Option<usize>,
    title: &str,
) -> (bool, Option<usize>, &str) {
    (traversal_index.is_none(), traversal_index, title)
}

// This struct represents a page in a wiki.
#[derive(Clone, Debug)]
pub struct Page {
    pub title: String,        // Non-empty, one line, trimmed, and not starting with `/`
    pub content: ContentText, // No leading or trailing whitespace, and lines are trimmed at the end
    pub links: Vec<Link>,
    pub traversal_index: Option<usize>, // Position in a depth-first traversal from the root
    pub source_range: SourceRange,      // The complete page without leading or trailing whitespace
    pub title_source_range: SourceRange, // From the title text, not the `#`, through the line's end
    pub has_syntax_errors: bool, // Whether the content has errors, so its links may not match it
}

impl Page {
    // Render the page for a Markdown preview, linking each link to the destination the callback
    // provides, if any. The prose around links becomes Markdown once Mull's escapes are removed.
    pub fn to_markdown<F>(&self, mut link_url: F) -> Markdown
    where
        F: FnMut(&Link) -> Option<String>,
    {
        // Render only the title of a page with syntax errors, since its content may have delimiters
        // which don't correspond to its links.
        let title = render_markdown_literal(&self.title).0;
        if self.has_syntax_errors {
            return Markdown(format!("{TITLE_PREFIX}{title}"));
        }

        // Render each parsed link with its semantic destination while retaining surrounding prose.
        let source = self.content.as_str();
        let mut content = String::new();
        let mut copied_through = 0;
        let mut link_start = None;
        let mut links = self.links.iter();
        for (index, character) in unescaped_characters(source) {
            // Track complete unescaped delimiter pairs, which are valid in a page without syntax
            // errors.
            match character {
                '[' => link_start = Some(index),
                ']' => {
                    // Copy the prose before the link, then render the link the parser recorded for
                    // this delimiter pair.
                    let start = link_start
                        .take()
                        .expect("A page without syntax errors should have balanced delimiters.");
                    content.push_str(
                        &render_markdown_before_link(&ContentText::from_source(
                            &source[copied_through..start],
                        ))
                        .0,
                    );
                    let link = links.next().expect(
                        "A page without syntax errors should have a link per delimiter pair.",
                    );
                    let url = link_url(link);
                    content.push_str(
                        &match link {
                            Link::Text { title, .. } => {
                                render_markdown_text_link(title, url.as_deref())
                            }
                            Link::Filesystem { target, .. } => {
                                render_markdown_filesystem_link(target, url.as_deref())
                            }
                        }
                        .0,
                    );
                    copied_through = index + character.len_utf8();
                }
                _ => {}
            }
        }
        content.push_str(&ContentText::from_source(&source[copied_through..]).unescape());

        // Preserve the same title-and-content shape as the Mull rendering without a trailing line.
        Markdown(if content.is_empty() {
            format!("{TITLE_PREFIX}{title}")
        } else {
            format!("{TITLE_PREFIX}{title}\n\n{content}")
        })
    }
}

// Render pages in the wiki's heading-and-content format.
impl fmt::Display for Page {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Omit the content separator when there's no content.
        if self.content.as_str().is_empty() {
            writeln!(formatter, "{TITLE_PREFIX}{}", self.title)
        } else {
            writeln!(
                formatter,
                "{TITLE_PREFIX}{}\n\n{}",
                self.title,
                self.content.as_str(),
            )
        }
    }
}

// This is text as it appears in a page's content, where `[` and `]` delimit links and a backslash
// escapes a following `[`, `]`, `#`, or backslash [ref:content_escapes]. Once the escapes are
// removed, the text around links is Markdown. A heading isn't content: a page's title appears in
// its heading as is.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContentText(String);

impl ContentText {
    // Escape plain text, such as a page title or a path, so it retains its meaning in content.
    pub fn escape(plain: &str) -> Self {
        Self(
            plain
                .replace('\\', "\\\\")
                .replace('[', "\\[")
                .replace(']', "\\]"),
        )
    }

    // Accept text that's already written as content, such as part of a wiki's source.
    pub fn from_source(source: &str) -> Self {
        Self(source.to_owned())
    }

    // Decode escaped characters into plain text, as the parser does.
    pub fn unescape(&self) -> String {
        let mut plain = String::new();
        let mut characters = self.0.chars().peekable();
        while let Some(character) = characters.next() {
            plain.push(if character == '\\' {
                characters
                    .next_if(|&next| is_escapable(next))
                    .unwrap_or(character)
            } else {
                character
            });
        }
        plain
    }

    // Expose the text as written, such as to scan it for delimiters.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    // Release the text as written, such as for an edit to the source.
    pub fn into_string(self) -> String {
        self.0
    }
}

// Scan content for the characters which aren't escaped, such as unescaped link delimiters. A
// backslash escapes a following `[`, `]`, `#`, or backslash [tag:content_escapes].
pub fn unescaped_characters(content: &str) -> impl Iterator<Item = (usize, char)> {
    let mut previous_was_escape = false;
    content.char_indices().filter(move |&(_, character)| {
        let is_escaped = previous_was_escape && is_escapable(character);
        previous_was_escape = character == '\\' && !is_escaped;
        !is_escaped
    })
}

// Determine whether a backslash escapes a character in content.
fn is_escapable(character: char) -> bool {
    matches!(character, '\\' | '[' | ']' | '#')
}

// These are the source occurrences through which a page can reference a target.
#[derive(Clone, Debug)]
pub enum Link {
    Text {
        title: String,
        source_range: SourceRange,
    },
    Filesystem {
        target: FilesystemTarget,
        source_range: SourceRange,
    },
}

impl Link {
    // Locate the link's source occurrence, whatever its kind.
    pub fn source_range(&self) -> SourceRange {
        match self {
            Link::Text { source_range, .. } | Link::Filesystem { source_range, .. } => {
                *source_range
            }
        }
    }
}

// This is the target of a filesystem link in canonical form, which parsing establishes once: a
// normalized path and whether it names a directory. The link text follows from those alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemTarget {
    path: PathBuf,
    is_directory: bool,
}

impl FilesystemTarget {
    // Parse the text of a filesystem link between its delimiters, which starts with `/` and names a
    // directory if it also ends with one. Describe any problem with a message.
    pub fn parse(text: &ContentText) -> Result<Self, String> {
        let text = text.unescape();
        let Some(path) = text.strip_prefix(FILESYSTEM_LINK_PREFIX) else {
            return Err(format!(
                "A filesystem link must start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ));
        };
        Self::from_name(path, text.ends_with(DIRECTORY_LINK_SUFFIX))
    }

    // Parse a path written without link syntax or escapes, such as a rename's new name, for a
    // target of the given kind. The path is relative to the files directory even if it starts
    // with `/`, and it must stay inside the logical files tree.
    pub fn from_name(path: &str, is_directory: bool) -> Result<Self, String> {
        // Reject components that escape the logical files tree
        // [tag:filesystem_path_components]. A root or prefix makes the path absolute.
        let path = Path::new(path.trim_start_matches('/'));
        if path
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
        {
            return Err(format!(
                "Path {} must be relative to the files directory.",
                path.code_str(),
            ));
        }

        // A parent component could lead outside the files directory.
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(format!(
                "Path {} must not contain {}.",
                path.code_str(),
                "..".code_str(),
            ));
        }

        // Normalize harmless current-directory components without resolving symlinks.
        Ok(Self::new(
            path.components()
                .filter_map(|component| match component {
                    Component::Normal(component) => Some(component),
                    Component::CurDir => None,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        // Escaping components were rejected above [ref:filesystem_path_components].
                        unreachable!("Filesystem link path components were already validated.")
                    }
                })
                .collect(),
            is_directory,
        ))
    }

    // Target the files directory itself.
    pub fn files_directory() -> Self {
        Self::new(PathBuf::new(), true)
    }

    // List the directories containing this target, from nearest to farthest, ending with the
    // files directory.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.path
            .ancestors()
            .skip(1)
            .map(|ancestor| Self::new(ancestor.to_owned(), true))
    }

    // Move this target from one directory to another, keeping its kind, if it's the directory or
    // lies within it.
    pub fn moved(&self, from: &FilesystemTarget, to: &FilesystemTarget) -> Option<Self> {
        let suffix = self.path.strip_prefix(&from.path).ok()?;
        Some(Self::new(to.path.join(suffix), self.is_directory))
    }

    // Write the link text: `/` and the path's components, followed by `/` for a directory, escaped
    // as content. This is the only place that decides how a link is written.
    pub fn text(&self) -> ContentText {
        // Join the components with the separator that links use on every platform. Link paths come
        // from UTF-8 text.
        let components = self
            .path
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .expect("Link paths should come from UTF-8 text.")
            })
            .collect::<Vec<_>>();
        let mut text = format!("{FILESYSTEM_LINK_PREFIX}{}", components.join("/"));
        if self.is_directory && !components.is_empty() {
            text.push_str(DIRECTORY_LINK_SUFFIX);
        }

        // Escape the finished text once, as it will appear in the source.
        ContentText::escape(&text)
    }

    // Expose the path relative to the files directory, without any root, prefix, `.`, or `..`
    // components. It's spelled as written, which may differ from the names on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Determine whether the target is the files directory itself.
    pub fn is_files_directory(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    // Determine whether the target is a directory.
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    // Create a target from a normalized path. The files directory is always a directory.
    fn new(path: PathBuf, is_directory: bool) -> Self {
        Self {
            is_directory: is_directory || path.as_os_str().is_empty(),
            path,
        }
    }
}

// Format a target for human-facing diagnostic output by its path as written.
impl CodeStr for FilesystemTarget {
    fn code_str(&self) -> ColoredString {
        code_file_path(&self.path)
    }
}

// This is Markdown for display. Only the rendering code in this module produces it, since it
// encodes whatever text it's given.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Markdown(String);

impl Markdown {
    // Release the Markdown for display.
    pub fn into_string(self) -> String {
        self.0
    }
}

// Render the prose before a link as Markdown which can't change the link, such as by escaping its
// first character or making it an image, by writing such a trailing character as a reference.
fn render_markdown_before_link(prose: &ContentText) -> Markdown {
    // Determine whether text ends with a backslash which would escape what follows.
    let ends_with_escape = |text: &str| {
        text.chars()
            .rev()
            .take_while(|&character| character == '\\')
            .count()
            % 2
            == 1
    };

    // Replace an escaping backslash or an unescaped `!` with the character it displays.
    let mut markdown = prose.unescape();
    if ends_with_escape(&markdown) {
        markdown.pop();
        markdown.push_str("&#92;");
    } else if let Some(before) = markdown.strip_suffix('!')
        && !ends_with_escape(before)
    {
        markdown.truncate(before.len());
        markdown.push_str("&#33;");
    }
    Markdown(markdown)
}

// Render a text link to the page with the given title as ordinary bracketed text with an optional
// Markdown destination.
fn render_markdown_text_link(title: &str, url: Option<&str>) -> Markdown {
    // Keep Markdown punctuation in page titles from changing the rendered label, and retain the
    // visible Mull delimiters inside the clickable region.
    let label = format!("&#91;{}&#93;", render_markdown_literal(title).0);
    Markdown(match url {
        Some(url) => format!("[{label}]({url})"),
        None => label,
    })
}

// Render a filesystem link as inline code without exposing Mull's escapes, linking it to an
// optional destination.
fn render_markdown_filesystem_link(target: &FilesystemTarget, url: Option<&str>) -> Markdown {
    // Use a fence longer than every backtick run occurring in the link text.
    let source = format!("[{}]", target.text().unescape());
    let longest_run = source
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .expect("Splitting text should yield at least one piece.");
    let fence = "`".repeat(longest_run + 1);

    // The surrounding brackets keep the content distinct from either side of the fence, and angle
    // brackets let the destination contain characters such as parentheses.
    let code = format!("{fence}{source}{fence}");
    Markdown(match url {
        Some(url) => format!("[{code}](<{url}>)"),
        None => code,
    })
}

// Render plain text literally in Markdown by replacing its syntax characters with entities. This
// includes `#`, since a trailing sequence of them would otherwise close a heading.
fn render_markdown_literal(text: &str) -> Markdown {
    let mut rendered = String::new();
    for character in text.chars() {
        rendered.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '\\' => "&#92;",
            '`' => "&#96;",
            '*' => "&#42;",
            '_' => "&#95;",
            '[' => "&#91;",
            ']' => "&#93;",
            '~' => "&#126;",
            '#' => "&#35;",
            _ => {
                rendered.push(character);
                continue;
            }
        });
    }
    Markdown(rendered)
}

#[cfg(test)]
mod tests {
    use super::{ContentText, FilesystemTarget, Link, Page, Wiki, title_line_text};
    use crate::error::SourceRange;
    use std::collections::HashMap;

    // Use a harmless range when testing rendering, which doesn't inspect source locations.
    const SOURCE_RANGE: SourceRange = SourceRange { start: 0, end: 0 };

    // Recognize a title marker followed by a space or the end of the line, but not other headings.
    #[test]
    fn title_lines() {
        assert_eq!(title_line_text("#"), Some(""));
        assert_eq!(title_line_text("# Greeting  "), Some("Greeting  "));
        assert_eq!(title_line_text("#Greeting"), None);
        assert_eq!(title_line_text("## Greeting"), None);
        assert_eq!(title_line_text(""), None);
    }

    // Ensure pages are rendered in the wiki's source format.
    #[test]
    fn page_display() {
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::from_source("Hello, world!"),
            links: Vec::new(),
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(page.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Ensure empty pages don't contain a redundant content separator.
    #[test]
    fn empty_page_display() {
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::default(),
            links: Vec::new(),
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(page.to_string(), "# Greeting\n");
    }

    // Remove Mull's escapes from prose in Markdown previews, leaving Markdown syntax such as a
    // link, while rendering Mull links.
    #[test]
    fn page_markdown() {
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::from_source(r"\[Text\](url), \\, \#, and [Home]."),
            links: vec![Link::Text {
                title: "Home".to_owned(),
                source_range: SOURCE_RANGE,
            }],
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(
            page.to_markdown(|link| match link {
                Link::Text { title, .. } if title == "Home" => {
                    Some("command:mull.revealRange?destination".to_owned())
                }
                Link::Text { .. } | Link::Filesystem { .. } => None,
            })
            .into_string(),
            concat!(
                "# Greeting\n\n[Text](url), \\, #, and ",
                "[&#91;Home&#93;](command:mull.revealRange?destination).",
            ),
        );
    }

    // Keep the prose before a link from escaping the link or making it an image, while leaving
    // escaped characters as they are.
    #[test]
    fn page_markdown_link_boundaries() {
        let home = Link::Text {
            title: "Home".to_owned(),
            source_range: SOURCE_RANGE,
        };
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::from_source(r"\\[Home] ![Home] \![Home] \\\\[Home]"),
            links: vec![home.clone(), home.clone(), home.clone(), home],
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(
            page.to_markdown(|_| Some("url".to_owned())).into_string(),
            concat!(
                "# Greeting\n\n&#92;[&#91;Home&#93;](url) &#33;[&#91;Home&#93;](url) ",
                r"\![&#91;Home&#93;](url) \\[&#91;Home&#93;](url)",
            ),
        );
    }

    // Render only the title of a page with syntax errors, whose delimiters may not match its links.
    #[test]
    fn page_markdown_syntax_errors() {
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::from_source("Unexpected] [Gree[ting]"),
            links: Vec::new(),
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: true,
        };

        assert_eq!(page.to_markdown(|_link| None).into_string(), "# Greeting");
    }

    // Render titles literally rather than as Markdown syntax.
    #[test]
    fn page_markdown_title() {
        let page = Page {
            title: "A*B* [C](d) <e> #".to_owned(),
            content: ContentText::default(),
            links: Vec::new(),
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(
            page.to_markdown(|_link| None).into_string(),
            "# A&#42;B&#42; &#91;C&#93;(d) &lt;e&gt; &#35;",
        );
    }

    // Keep unresolved text links visible but non-clickable in Markdown previews.
    #[test]
    fn unresolved_page_markdown_link() {
        let page = Page {
            title: "Greeting".to_owned(),
            content: ContentText::from_source("See [Missing]."),
            links: vec![Link::Text {
                title: "Missing".to_owned(),
                source_range: SOURCE_RANGE,
            }],
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(
            page.to_markdown(|_link| None).into_string(),
            "# Greeting\n\nSee &#91;Missing&#93;.",
        );
    }

    // Distinguish file and directory links from prose with Markdown code styling, linking those
    // with destinations.
    #[test]
    fn filesystem_link_markdown() {
        let page = Page {
            title: "Files".to_owned(),
            content: ContentText::from_source("[/notes.txt] and [/odd`name/]"),
            links: vec![
                Link::Filesystem {
                    target: FilesystemTarget::parse(&ContentText::from_source("/notes.txt"))
                        .unwrap(),
                    source_range: SOURCE_RANGE,
                },
                Link::Filesystem {
                    target: FilesystemTarget::parse(&ContentText::from_source("/odd`name/"))
                        .unwrap(),
                    source_range: SOURCE_RANGE,
                },
            ],
            traversal_index: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
            has_syntax_errors: false,
        };

        assert_eq!(
            page.to_markdown(|link| match link {
                Link::Filesystem { target, .. } if !target.is_directory() => {
                    Some("file:///wiki/notes.txt".to_owned())
                }
                Link::Text { .. } | Link::Filesystem { .. } => None,
            })
            .into_string(),
            "# Files\n\n[`[/notes.txt]`](<file:///wiki/notes.txt>) and ``[/odd`name/]``",
        );
    }

    // Establish a filesystem link's canonical text when parsing it, which parses back to itself.
    #[test]
    fn filesystem_target_canonical_text() {
        for (text, canonical, is_directory) in [
            ("/notes.txt", "/notes.txt", false),
            ("//images/./raw//", "/images/raw/", true),
            ("/a\\[1\\]/b.txt", "/a\\[1\\]/b.txt", false),
            ("/", "/", true),
            ("/.", "/", true),
        ] {
            let target = FilesystemTarget::parse(&ContentText::from_source(text)).unwrap();
            assert_eq!(target.text().as_str(), canonical);
            assert_eq!(target.is_directory(), is_directory);
            assert_eq!(FilesystemTarget::parse(&target.text()).unwrap(), target);
        }
    }

    // Escape delimiters and backslashes in plain text, and decode only Mull's escapes, leaving any
    // other backslash as is.
    #[test]
    fn content_text_escaping() {
        assert_eq!(ContentText::escape(r"a\[b]\c\").as_str(), r"a\\\[b\]\\c\\");
        assert_eq!(
            ContentText::from_source(r"a\\\[b\]\\c\\").unescape(),
            r"a\[b]\c\",
        );
        assert_eq!(ContentText::from_source(r"\a\\\").unescape(), r"\a\\");
        assert_eq!(ContentText::from_source(r"\#a\\#").unescape(), r"#a\#");
        assert_eq!(ContentText::escape(r"\#").unescape(), r"\#");
    }

    // Reject filesystem link paths which would escape the wiki's logical tree.
    #[test]
    fn filesystem_target_escaping_paths() {
        assert!(FilesystemTarget::parse(&ContentText::from_source("/../notes.txt")).is_err());
        assert!(FilesystemTarget::parse(&ContentText::from_source("notes.txt")).is_err());
    }

    // Write a name without link syntax in the canonical form for the given kind, escaping any link
    // delimiters.
    #[test]
    fn filesystem_target_from_name() {
        let target = FilesystemTarget::from_name("photos/a[1]/", false).unwrap();
        assert_eq!(target.text().as_str(), "/photos/a\\[1\\]");
        assert_eq!(
            FilesystemTarget::from_name("photos", true)
                .unwrap()
                .text()
                .as_str(),
            "/photos/",
        );
    }

    // Move a target along with a directory containing it, keeping its kind.
    #[test]
    fn filesystem_target_moved() {
        let directory = FilesystemTarget::parse(&ContentText::from_source("/photos/")).unwrap();
        let destination =
            FilesystemTarget::parse(&ContentText::from_source("/archive/photos/")).unwrap();
        let moved = |text| {
            FilesystemTarget::parse(&ContentText::from_source(text))
                .unwrap()
                .moved(&directory, &destination)
                .map(|target| target.text().into_string())
        };
        assert_eq!(moved("/photos/"), Some("/archive/photos/".to_owned()));
        assert_eq!(
            moved("/photos/cat.jpg"),
            Some("/archive/photos/cat.jpg".to_owned()),
        );
        assert_eq!(moved("/photographs/cat.jpg"), None);
    }

    // Ensure a wiki containing an empty page has only its trailing line break.
    #[test]
    fn empty_page_wiki_display() {
        let wiki = Wiki {
            pages: HashMap::from([(
                "Greeting".to_owned(),
                Page {
                    title: "Greeting".to_owned(),
                    content: ContentText::default(),
                    links: Vec::new(),
                    traversal_index: None,
                    source_range: SOURCE_RANGE,
                    title_source_range: SOURCE_RANGE,
                    has_syntax_errors: false,
                },
            )]),
        };

        assert_eq!(wiki.to_string(), "# Greeting\n");
    }

    // Render pages in traversal order, placing pages without a traversal index last.
    #[test]
    fn wiki_display() {
        let wiki = Wiki {
            pages: HashMap::from([
                (
                    "Greeting".to_owned(),
                    Page {
                        title: "Greeting".to_owned(),
                        content: ContentText::from_source("Hello, world!"),
                        links: Vec::new(),
                        traversal_index: Some(1),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                        has_syntax_errors: false,
                    },
                ),
                (
                    "Home".to_owned(),
                    Page {
                        title: "Home".to_owned(),
                        content: ContentText::from_source("Check out the [Greeting]."),
                        links: Vec::new(),
                        traversal_index: Some(0),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                        has_syntax_errors: false,
                    },
                ),
                (
                    "Orphan".to_owned(),
                    Page {
                        title: "Orphan".to_owned(),
                        content: ContentText::default(),
                        links: Vec::new(),
                        traversal_index: None,
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                        has_syntax_errors: false,
                    },
                ),
            ]),
        };

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\nCheck out the [Greeting].\n\n",
                "# Greeting\n\nHello, world!\n\n",
                "# Orphan\n",
            ),
        );
    }

    // Ensure an empty wiki has no output.
    #[test]
    fn empty_wiki_display() {
        assert_eq!(Wiki::default().to_string(), "");
    }
}
