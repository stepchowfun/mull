use crate::{
    error::SourceRange,
    escaping::{Context, escape},
    format::CodeStr,
    spelled_path::code_file_path,
};
use colored::ColoredString;
use serde::{Deserialize, Serialize, Serializer};
use std::{
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
};

// These strings define the wiki format's extension, the suffix that names a wiki's file
// root, and structural markers. A link whose target starts with `/` is a filesystem link,
// relative to the file root, which names a directory if it ends with `/`. The home page's title
// identifies the root of every wiki's text-link graph.
pub const WIKI_EXTENSION: &str = "mull";
pub const FILE_ROOT_SUFFIX: &str = "_files";
pub const TITLE_PREFIX: &str = "# ";
pub const HOME_TITLE: &str = "Home";
pub const FILESYSTEM_LINK_PREFIX: &str = "/";
pub const DIRECTORY_LINK_SUFFIX: &str = "/";

// A line which starts with at least this many colons is a fence: an opening fence names a verb and
// its argument, and a closing fence consists of colons alone. The only verb is the one which embeds
// a page.
pub const MIN_FENCE_LENGTH: usize = 3;
pub const EMBED_VERB: &str = "embed";

// Find the text after a title line's prefix, which is a title marker followed by a space. A line
// that isn't a title line has none, including a bare title marker.
pub fn title_line_text(line: &str) -> Option<&str> {
    line.strip_prefix(TITLE_PREFIX)
}

// These are the kinds of fence lines. An opening fence's verb and argument are located by their
// offsets within the line, and its argument is trimmed.
#[derive(Debug, Eq, PartialEq)]
pub enum FenceLine<'a> {
    Opening {
        length: usize,
        verb: &'a str,
        argument: (usize, usize),
    },
    Closing {
        length: usize,
    },
}

// Read a line as a fence line: a run of fence characters, alone for a closing fence, or followed by
// whitespace, a verb, and an argument for an opening fence. Any other line, including one in which
// the run is followed directly by other text, isn't a fence line.
pub fn fence_line(line: &str) -> Option<FenceLine<'_>> {
    // Measure the run, which must be long enough.
    let length = line.len() - line.trim_start_matches(':').len();
    if length < MIN_FENCE_LENGTH {
        return None;
    }

    // Find what follows the run, which is either nothing or whitespace and a verb.
    let rest = &line[length..];
    if rest.trim().is_empty() {
        return Some(FenceLine::Closing { length });
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let verb_start = line.len() - rest.trim_start().len();
    let verb_end = line[verb_start..]
        .find(char::is_whitespace)
        .map_or(line.len(), |index| verb_start + index);
    let argument_start = line.len() - line[verb_end..].trim_start().len();
    let argument_end = line.trim_end().len().max(argument_start);
    Some(FenceLine::Opening {
        length,
        verb: &line[verb_start..verb_end],
        argument: (argument_start, argument_end),
    })
}

// This struct represents a parsed wiki, along with the source it was parsed from, which its pages'
// source ranges address and which it's rendered from.
#[derive(Clone, Debug)]
pub struct Wiki {
    pub pages: HashMap<PageTitle, Page>,
    source: String,
}

impl Wiki {
    // Start a wiki without any pages for the given source.
    pub fn new(source: &str) -> Self {
        Self {
            pages: HashMap::new(),
            source: source.to_owned(),
        }
    }

    // Iterate over the links of every page, in no particular order.
    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.pages.values().flat_map(|page| &page.links)
    }

    // Look up a page by its title as written in its title line, for tests.
    #[cfg(test)]
    pub fn page(&self, title: &str) -> &Page {
        &self.pages[&PageTitle::parse(title).expect("The title should be valid.")]
    }

    // Order pages as the wiki is rendered: reachable pages by their traversal index, followed by
    // unreachable pages in title order.
    pub fn rendering_order_key(
        traversal_index: Option<usize>,
        title: &PageTitle,
    ) -> (bool, Option<usize>, &PageTitle) {
        (traversal_index.is_none(), traversal_index, title)
    }

    // Render a page's content from the source, with each link written in its canonical form, each
    // embed refreshed, Unix line endings, and no whitespace at the end of a line. The pages being
    // rendered, which embed this one, are listed so a cycle of embeds is left as written.
    fn rendered_content(&self, page: &Page, embedding_pages: &mut Vec<PageTitle>) -> String {
        // Find the content after the title line, skipping the blank lines around it but keeping the
        // indentation of its first line, so an indented line which starts with a title marker
        // doesn't become a title.
        let content_end = page.source_range.end;
        let after_title = &self.source[page.title_source_range.end..content_end];
        let Some(first_character) = after_title.find(|character: char| !character.is_whitespace())
        else {
            return String::new();
        };
        let content_start = page.title_source_range.end
            + after_title[..first_character]
                .rfind('\n')
                .map_or(0, |index| index + '\n'.len_utf8());

        // Copy the content, replacing each embed with a fresh copy of the embedded page's content.
        // An embed of a missing page, or one which would complete a cycle, is copied as written.
        let mut content = String::new();
        let mut copied_through = content_start;
        embedding_pages.push(page.title.clone());
        for embed in &page.embeds {
            let Some(embedded_page) = self
                .pages
                .get(&embed.title)
                .filter(|embedded_page| !embedding_pages.contains(&embedded_page.title))
            else {
                continue;
            };
            self.copy_links(page, copied_through, embed.source_range.start, &mut content);
            let body = self.rendered_content(embedded_page, embedding_pages);
            let longest_run = body
                .lines()
                .map(|line| line.len() - line.trim_start_matches(':').len())
                .max()
                .unwrap_or(0);
            let fence = ":".repeat((longest_run + 1).max(MIN_FENCE_LENGTH));
            for part in [
                &fence,
                " ",
                EMBED_VERB,
                " [",
                &embed.title.text(Context::Link),
                "]\n",
            ] {
                content.push_str(part);
            }
            if !body.is_empty() {
                content.push_str(&body);
                content.push('\n');
            }
            content.push_str(&fence);
            copied_through = embed.source_range.end;
        }
        embedding_pages.pop();
        self.copy_links(page, copied_through, content_end, &mut content);

        // Use Unix line endings and remove whitespace at the end of each line. Links can't contain
        // line breaks, and each ends with a delimiter, so this never changes one.
        content
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Copy part of a page's content, replacing each link within it with its canonical form: a text
    // link's title escaped, and a filesystem link's normalized target. A malformed link, which a
    // wiki without syntax errors doesn't have, is copied as written.
    fn copy_links(&self, page: &Page, start: usize, end: usize, content: &mut String) {
        let mut links = page
            .links
            .iter()
            .filter(|link| start <= link.source_range().start && link.source_range().end <= end)
            .collect::<Vec<_>>();
        links.sort_by_key(|link| link.source_range().start);
        let mut copied_through = start;
        for link in links {
            let source_range = link.source_range();
            content.push_str(&self.source[copied_through..source_range.start]);
            let target = match link {
                Link::Text { title, .. } => title.text(Context::Link),
                Link::Filesystem { target, .. } => target.text(Context::Link),
                Link::Malformed { .. } => {
                    content.push_str(&self.source[source_range.start..source_range.end]);
                    copied_through = source_range.end;
                    continue;
                }
            };
            content.push('[');
            content.push_str(&target);
            content.push(']');
            copied_through = source_range.end;
        }
        content.push_str(&self.source[copied_through..end]);
    }
}

// Render pages deterministically in traversal order, with unreachable pages last in title order.
// Rendering assumes the wiki has no syntax errors, so none of a page's links are malformed.
impl fmt::Display for Wiki {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort reachable pages by traversal order, followed by unreachable pages in title order.
        let mut pages = self.pages.iter().collect::<Vec<_>>();
        pages.sort_by_key(|(title, page)| Self::rendering_order_key(page.traversal_index, title));

        // Write each page as a heading and its content, omitting the content separator when there's
        // no content. Add one line break between pages because each page already ends with one.
        for (index, (_title, page)) in pages.into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            let title = page.title.text(Context::Verbatim);
            let content = self.rendered_content(page, &mut Vec::new());
            if content.is_empty() {
                writeln!(formatter, "{TITLE_PREFIX}{title}")?;
            } else {
                writeln!(formatter, "{TITLE_PREFIX}{title}\n\n{content}")?;
            }
        }

        // Rendering succeeded.
        Ok(())
    }
}

// This struct represents a page in a wiki.
#[derive(Clone, Debug)]
pub struct Page {
    pub title: PageTitle,
    pub links: Vec<Link>,
    pub embeds: Vec<Embed>, // In source order, excluding embeds within the copies made by others
    pub traversal_index: Option<usize>, // Position in a depth-first traversal from the root
    pub source_range: SourceRange, // The complete page without leading or trailing whitespace
    pub title_source_range: SourceRange, // From the title text, not the `#`, through the line's end
}

// This struct represents a page embedded in another page's content. The embedding page shows a copy
// of the embedded page's content, which formatting refreshes along with any embeds within it.
#[derive(Clone, Debug)]
pub struct Embed {
    pub title: PageTitle,
    pub source_range: SourceRange, // From the opening fence through the end of the closing fence
    pub link_source_range: SourceRange, // The link in the opening fence which names the page
}

// This is a page's title, which text links name it by: non-empty, one line, trimmed, and not
// starting with `/`, which would make a link to it a filesystem link. It travels to the editor and
// back as JSON in quick fixes, so it's checked again when it's read back.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(try_from = "String")]
pub struct PageTitle(String);

impl PageTitle {
    // Make a title from text such as a title line's or a rename's new name, trimming it and
    // describing any rule it breaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.contains(['\r', '\n']) {
            return Err("A page title can't contain a line break.".to_owned());
        }
        let title = text.trim();
        if title.is_empty() {
            Err("A page title can't be empty.".to_owned())
        } else if title.starts_with(FILESYSTEM_LINK_PREFIX) {
            Err(format!(
                "A page title can't start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ))
        } else {
            Ok(Self(title.to_owned()))
        }
    }

    // Name the root of every wiki's text-link graph.
    pub fn home() -> Self {
        Self(HOME_TITLE.to_owned())
    }

    // Write the title as it appears in a context, such as escaped in a link or as written in a
    // title line.
    pub fn text(&self, context: Context) -> String {
        escape(&self.0, context)
    }
}

// Read a title back from JSON, checking it as when it was first made.
impl TryFrom<String> for PageTitle {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

// Write a title to JSON as written in its title line.
impl Serialize for PageTitle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

// Format a title for a message, as written in its title line.
impl CodeStr for PageTitle {
    fn code_str(&self) -> ColoredString {
        self.0.code_str()
    }
}

// These are the source occurrences through which a page can reference a target. A malformed link
// has a syntax error, such as a missing closing delimiter, so it references nothing, but an editor
// can still complete it. A link which isn't closed extends to the end of its line.
#[derive(Clone, Debug)]
pub enum Link {
    Text {
        title: PageTitle,
        source_range: SourceRange,
    },
    Filesystem {
        target: FilesystemTarget,
        source_range: SourceRange,
    },
    Malformed {
        source_range: SourceRange,
        is_closed: bool,
    },
}

impl Link {
    // Locate the link's source occurrence, whatever its kind.
    pub fn source_range(&self) -> SourceRange {
        match self {
            Link::Text { source_range, .. }
            | Link::Filesystem { source_range, .. }
            | Link::Malformed { source_range, .. } => *source_range,
        }
    }

    // Locate the link's closing delimiter, if it has one.
    pub fn closing_delimiter(&self) -> Option<usize> {
        let is_closed = match self {
            Link::Text { .. } | Link::Filesystem { .. } => true,
            Link::Malformed { is_closed, .. } => *is_closed,
        };
        is_closed.then(|| self.source_range().end - ']'.len_utf8())
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
    // Parse the unescaped text of a filesystem link between its delimiters, which starts with `/`
    // and names a directory if it also ends with one. Describe any problem with a message.
    pub fn parse(text: &str) -> Result<Self, String> {
        let Some(path) = text.strip_prefix(FILESYSTEM_LINK_PREFIX) else {
            return Err(format!(
                "A filesystem link must start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ));
        };
        Self::from_name(path, text.ends_with(DIRECTORY_LINK_SUFFIX))
    }

    // Parse a path written without link syntax or escapes, such as a rename's new name, for a
    // target of the given kind. The path is relative to the file root even if it starts
    // with `/`, and it must stay inside the logical file tree.
    pub fn from_name(path: &str, is_directory: bool) -> Result<Self, String> {
        // Reject components that escape the logical file tree
        // [tag:filesystem_path_components]. A root or prefix makes the path absolute.
        let path = Path::new(path.trim_start_matches('/'));
        if path
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
        {
            return Err(format!("Path {} can't be absolute.", path.code_str()));
        }

        // A parent component could lead outside the file root.
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

    // Target the file root itself.
    pub fn file_root() -> Self {
        Self::new(PathBuf::new(), true)
    }

    // Create a target from a normalized path. The file root is always a directory.
    fn new(path: PathBuf, is_directory: bool) -> Self {
        Self {
            is_directory: is_directory || path.as_os_str().is_empty(),
            path,
        }
    }

    // Expose the path relative to the file root, without any root, prefix, `.`, or `..`
    // components. It's spelled as written, which may differ from the names on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Determine whether the target is a directory.
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    // Determine whether the target is the file root itself.
    pub fn is_file_root(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    // List the directories containing this target, from nearest to farthest, ending with the
    // file root.
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
    // for a context. This is the only place that decides how a link is written.
    pub fn text(&self, context: Context) -> String {
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
        escape(&text, context)
    }
}

// Format a target for human-facing diagnostic output by its path as written.
impl CodeStr for FilesystemTarget {
    fn code_str(&self) -> ColoredString {
        code_file_path(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::{FenceLine, FilesystemTarget, PageTitle, Wiki, fence_line, title_line_text};
    use crate::{
        escaping::{Context, unescape},
        line_index::LineIndex,
        parser::parse,
    };

    // Parse a wiki without syntax errors for rendering.
    fn parse_wiki(source: &str) -> Wiki {
        let (wiki, errors) = parse(None, source, &LineIndex::new(source));
        assert!(errors.is_empty());
        wiki
    }

    // Recognize a title marker followed by a space, but not a bare title marker or other headings.
    #[test]
    fn title_lines() {
        assert_eq!(title_line_text("# "), Some(""));
        assert_eq!(title_line_text("# Greeting  "), Some("Greeting  "));
        assert_eq!(title_line_text("#"), None);
        assert_eq!(title_line_text("#\tGreeting"), None);
        assert_eq!(title_line_text("#Greeting"), None);
        assert_eq!(title_line_text("## Greeting"), None);
        assert_eq!(title_line_text(""), None);
    }

    // Render pages in traversal order, placing unreachable pages last.
    #[test]
    fn wiki_display() {
        let wiki = parse_wiki(concat!(
            "# Orphan\n",
            "# Greeting\n\nHello, world!\n",
            "# Home\n\nCheck out the [Greeting].",
        ));

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
        assert_eq!(Wiki::new("").to_string(), "");
    }

    // Ensure a wiki containing an empty page has only its trailing line break, without a redundant
    // content separator.
    #[test]
    fn empty_page_wiki_display() {
        assert_eq!(parse_wiki("# Greeting\n\n\n").to_string(), "# Greeting\n");
    }

    // Write content as written, apart from links in their canonical forms, blank lines around the
    // content, and whitespace at the ends of lines, keeping the indentation of the first line.
    #[test]
    fn content_display() {
        let wiki = parse_wiki(concat!(
            "# Home\r\n\r\n\r\n  # Indented  \r\n",
            "See [ Other ] and [//images/./raw//].  \r\n\r\n",
            "\tDone.\r\n\r\n",
            "# Other",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  # Indented\nSee [Other] and [/images/raw/].\n\n\tDone.\n\n",
                "# Other\n",
            ),
        );
    }

    // Recognize opening fences with a verb and a trimmed argument, and closing fences, but not
    // shorter runs or runs followed directly by other text.
    #[test]
    fn fence_lines() {
        assert_eq!(
            fence_line("::: embed [Page]  "),
            Some(FenceLine::Opening {
                length: 3,
                verb: "embed",
                argument: (10, 16),
            }),
        );
        assert_eq!(
            fence_line("::::\tembed"),
            Some(FenceLine::Opening {
                length: 4,
                verb: "embed",
                argument: (10, 10),
            }),
        );
        assert_eq!(fence_line(":::"), Some(FenceLine::Closing { length: 3 }));
        assert_eq!(
            fence_line("::::: \t"),
            Some(FenceLine::Closing { length: 5 }),
        );
        assert_eq!(fence_line(":: embed [Page]"), None);
        assert_eq!(fence_line(":::embed [Page]"), None);
        assert_eq!(fence_line(" ::: embed [Page]"), None);
    }

    // Refresh each embed with a copy of the embedded page's content, including the embeds in it,
    // with a fence longer than any in the copy. An embed of a missing page or one which would
    // complete a cycle is left as written.
    #[test]
    fn embeds_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n::: embed [ Other ]\nstale\n:::\n\n",
            "::: embed [Missing]\nkept\n:::\n\n",
            "::: embed [Home]\nkept\n:::\n\n",
            "# Other\n\nSee [ Last ].\n\n::: embed [Last]\n:::\n\n",
            "# Last\n\nThe end.",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n",
                ":::: embed [Other]\nSee [Last].\n\n::: embed [Last]\nThe end.\n:::\n::::\n\n",
                "::: embed [Missing]\nkept\n:::\n\n",
                "::: embed [Home]\nkept\n:::\n\n",
                "# Other\n\nSee [Last].\n\n::: embed [Last]\nThe end.\n:::\n\n",
                "# Last\n\nThe end.\n",
            ),
        );
    }

    // Trim a page title, and reject one which is empty, spans lines, or starts with `/`.
    #[test]
    fn page_titles() {
        assert_eq!(
            PageTitle::parse("  Greeting  ")
                .unwrap()
                .text(Context::Verbatim),
            "Greeting",
        );
        assert_eq!(
            PageTitle::parse("  ").unwrap_err(),
            "A page title can't be empty.",
        );
        assert_eq!(
            PageTitle::parse("Two\nlines").unwrap_err(),
            "A page title can't contain a line break.",
        );
        assert!(
            PageTitle::parse("/notes")
                .unwrap_err()
                .starts_with("A page title can't start with"),
        );
    }

    // Check a page title again when reading it back from JSON.
    #[test]
    fn page_title_json() {
        let title = PageTitle::parse("Greeting").unwrap();
        let json = serde_json::to_value(&title).unwrap();

        assert_eq!(json, "Greeting");
        assert_eq!(serde_json::from_value::<PageTitle>(json).unwrap(), title);
        assert!(serde_json::from_value::<PageTitle>("/notes".into()).is_err());
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
            let target = FilesystemTarget::parse(&unescape(text, Context::Link).unwrap()).unwrap();
            assert_eq!(target.text(Context::Link), canonical);
            assert_eq!(target.is_directory(), is_directory);
            assert_eq!(
                FilesystemTarget::parse(
                    &unescape(&target.text(Context::Link), Context::Link).unwrap(),
                )
                .unwrap(),
                target,
            );
        }
    }

    // Reject filesystem link paths which would escape the wiki's logical tree.
    #[test]
    fn filesystem_target_escaping_paths() {
        assert!(FilesystemTarget::parse("/../notes.txt").is_err());
        assert!(FilesystemTarget::parse("notes.txt").is_err());
    }

    // Write a name without link syntax in the canonical form for the given kind, escaping any link
    // delimiters.
    #[test]
    fn filesystem_target_from_name() {
        let target = FilesystemTarget::from_name("photos/a[1]/", false).unwrap();
        assert_eq!(target.text(Context::Link), "/photos/a\\[1\\]");
        assert_eq!(
            FilesystemTarget::from_name("photos", true)
                .unwrap()
                .text(Context::Link),
            "/photos/",
        );
    }

    // Move a target along with a directory containing it, keeping its kind.
    #[test]
    fn filesystem_target_moved() {
        let directory = FilesystemTarget::parse("/photos/").unwrap();
        let destination = FilesystemTarget::parse("/archive/photos/").unwrap();
        let moved = |text| {
            FilesystemTarget::parse(text)
                .unwrap()
                .moved(&directory, &destination)
                .map(|target| target.text(Context::Link))
        };
        assert_eq!(moved("/photos/"), Some("/archive/photos/".to_owned()));
        assert_eq!(
            moved("/photos/cat.jpg"),
            Some("/archive/photos/cat.jpg".to_owned()),
        );
        assert_eq!(moved("/photographs/cat.jpg"), None);
    }
}
