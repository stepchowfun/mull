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

// A line which starts with at least this many colons, after any indentation, is a fence: an opening
// fence names the page it embeds or the script which computes its body, and a closing fence
// consists of colons alone.
pub const MIN_FENCE_LENGTH: usize = 3;

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

    // Render a page's content in its canonical form, with each embed refreshed.
    pub fn page_content(&self, page: &Page) -> String {
        self.rendered_content(page, &mut Vec::new())
    }

    // Render a page's content from the source in its canonical form, with each embed refreshed. The
    // pages being rendered, which embed this one, are listed so a cycle of embeds is left as
    // written.
    fn rendered_content(&self, page: &Page, embedding_pages: &mut Vec<PageTitle>) -> String {
        embedding_pages.push(page.title.clone());
        let content = self.canonical_content(page, |embed| {
            self.pages
                .get(&embed.title)
                .filter(|embedded_page| !embedding_pages.contains(&embedded_page.title))
                .map(|embedded_page| self.rendered_content(embedded_page, embedding_pages))
        });
        embedding_pages.pop();
        content
    }

    // Render a page's own content in its canonical form, leaving out the copies its embeds show,
    // which belong to the embedded pages. Instances of a page with the same own content differ
    // only in ways formatting erases.
    pub fn own_content(&self, page: &Page) -> String {
        self.canonical_content(page, |_embed| Some(String::new()))
    }

    // Render a page's content from the source, with each link written in its canonical form, Unix
    // line endings, and no whitespace at the end of a line. Each embed shows the body given for it,
    // with each line indented like its opening fence, or is copied as written if it has none.
    fn canonical_content(
        &self,
        page: &Page,
        mut body: impl FnMut(&Embed) -> Option<String>,
    ) -> String {
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

        // Copy the content, writing each embed with its body and a fence longer than any in it.
        let mut content = String::new();
        let mut copied_through = content_start;
        for embed in &page.embeds {
            let Some(body) = body(embed) else {
                continue;
            };
            self.copy_links(page, copied_through, embed.source_range.start, &mut content);
            let indentation = self.indentation(embed.source_range.start);
            let longest_run = body
                .lines()
                .map(str::trim_start)
                .map(|line| line.len() - line.trim_start_matches(':').len())
                .max()
                .unwrap_or(0);
            let fence = ":".repeat((longest_run + 1).max(MIN_FENCE_LENGTH));
            for part in [&fence, " [", &embed.title.text(Context::Link), "]\n"] {
                content.push_str(part);
            }
            for line in body.lines() {
                content.push_str(indentation);
                content.push_str(line);
                content.push('\n');
            }
            content.push_str(indentation);
            content.push_str(&fence);
            copied_through = embed.source_range.end;
        }
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

    // Find the body of a block, such as the copy an embed shows, with the indentation of its
    // opening fence removed from each line, along with where each line of the body starts in it and
    // in the source. A block written on one line has no body.
    pub fn body(&self, block: SourceRange) -> Option<(String, Vec<(usize, usize)>)> {
        let block_source = &self.source[block.start..block.end];
        let body_start = block_source.find('\n')? + '\n'.len_utf8();
        let body_end = block_source.rfind('\n')? + '\n'.len_utf8();
        let indentation = self.indentation(block.start);
        let mut body = String::new();
        let mut line_starts = Vec::new();
        let mut line_source_start = block.start + body_start;
        for line in block_source[body_start..body_end].split_inclusive('\n') {
            let unindented_line = line
                .strip_prefix(indentation)
                .unwrap_or_else(|| line.trim_start_matches([' ', '\t']));
            line_starts.push((
                body.len(),
                line_source_start + line.len() - unindented_line.len(),
            ));
            body.push_str(unindented_line);
            line_source_start += line.len();
        }
        Some((body, line_starts))
    }

    // Find the indentation of the opening fence of a block which starts at an offset.
    pub fn indentation(&self, block_start: usize) -> &str {
        &self.source[self.source[..block_start]
            .rfind('\n')
            .map_or(0, |index| index + '\n'.len_utf8())..block_start]
    }
}

// Render pages deterministically in traversal order, with unreachable pages last in title order.
// Rendering assumes the wiki has no syntax errors, so none of a page's links are malformed. It
// refreshes each copy from its page, so any changes to copies must be propagated to their pages
// first.
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
            let content = self.page_content(page);
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
    pub script_blocks: Vec<ScriptBlock>, // In source order, excluding those within other blocks
    pub traversal_index: Option<usize>, // Position in a depth-first traversal from the root
    pub source_range: SourceRange, // The complete page without leading or trailing whitespace
    pub title_source_range: SourceRange, // From the title text, not the `#`, through the line's end
}

// This struct represents a page embedded in another page's content. The embedding page shows a copy
// of the embedded page's content, which formatting keeps in sync with the page, along with any
// embeds within it.
#[derive(Clone, Debug)]
pub struct Embed {
    pub title: PageTitle,
    pub source_range: SourceRange, // From the opening fence's colons through the closing fence
    pub link_source_range: SourceRange, // The link in the opening fence which names the page
}

// This struct represents a block whose body is computed by a script, which reads the body and
// writes its replacement. The script is named in the opening fence, followed by its arguments. The
// closing fence can be followed by a hash of the script, the arguments, and the body, which notes
// that the body is the script's output, so the script needn't run again until one of them changes.
#[derive(Clone, Debug)]
pub struct ScriptBlock {
    pub name: String,
    pub source_range: SourceRange, // From the opening fence's colons through the closing fence
    pub name_source_range: SourceRange, // The script's name in the opening fence
    pub arguments_source_range: SourceRange, // The trimmed text after the name
    pub hash: Option<String>,      // After the `#` following the closing fence, if any
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
    use super::{FilesystemTarget, PageTitle, Wiki};
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

    // Refresh each embed with a copy of the embedded page's content, including the embeds in it,
    // with a fence longer than any in the copy. An embed of a missing page or one which would
    // complete a cycle is left as written.
    #[test]
    fn embeds_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n::: [ Other ]\nstale\n:::\n\n",
            "::: [Missing]\nkept\n:::\n\n",
            "::: [Home]\nkept\n:::\n\n",
            "# Other\n\nSee [ Last ].\n\n::: [Last]\n:::\n\n",
            "# Last\n\nThe end.",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n",
                ":::: [Other]\nSee [Last].\n\n::: [Last]\nThe end.\n:::\n::::\n\n",
                "::: [Missing]\nkept\n:::\n\n",
                "::: [Home]\nkept\n:::\n\n",
                "# Other\n\nSee [Last].\n\n::: [Last]\nThe end.\n:::\n\n",
                "# Last\n\nThe end.\n",
            ),
        );
    }

    // Indent each line of an embed's copy, including its closing fence, like its opening fence,
    // leaving blank lines empty, and indent nested embeds further.
    #[test]
    fn indented_embeds_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n- Item\n  ::: [Other]\nstale\n:::\n\n",
            "# Other\n\nSee:\n\n\t::: [Last]\n\t:::\n\n",
            "# Last\n\nThe end.",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n- Item\n",
                "  :::: [Other]\n  See:\n\n  \t::: [Last]\n  \tThe end.\n  \t:::\n",
                "  ::::\n\n",
                "# Other\n\nSee:\n\n\t::: [Last]\n\tThe end.\n\t:::\n\n",
                "# Last\n\nThe end.\n",
            ),
        );
    }

    // Render a page's own content in its canonical form, leaving out the copies its embeds show.
    #[test]
    fn own_content() {
        let wiki = parse_wiki(concat!(
            "# Home\n\nSee [ Other ].  \r\n\n",
            "  :::: [Other]\n  Stale.\n  ::: [Last]\n  :::\n  ::::\n",
            "# Other\n# Last",
        ));

        assert_eq!(
            wiki.own_content(&wiki.pages[&PageTitle::parse("Home").unwrap()]),
            "See [Other].\n\n  ::: [Other]\n  :::",
        );
    }

    // Find a block's body without its indentation, with where each line starts in the body and in
    // the source. A line indented less than the opening fence loses its indentation, and a block
    // written on one line has no body.
    #[test]
    fn bodies() {
        let source = "# Home\n  ::: [Other]\n  First\n\n Second\n  :::\n::: [Other] :::\n# Other";
        let wiki = parse_wiki(source);
        let embeds = &wiki.pages[&PageTitle::parse("Home").unwrap()].embeds;

        assert_eq!(
            wiki.body(embeds[0].source_range),
            Some((
                "First\n\nSecond\n".to_owned(),
                vec![(0, 23), (6, 29), (7, 31)],
            )),
        );
        assert_eq!(&source[23..28], "First");
        assert_eq!(&source[31..37], "Second");
        assert_eq!(wiki.body(embeds[1].source_range), None);
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
