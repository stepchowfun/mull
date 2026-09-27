use crate::{
    error::SourceRange,
    format::{CodePath, CodeStr},
};
use colored::ColoredString;
use std::{
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
};

// These strings define the wiki format's extension and structural markers. A link whose target
// starts with `/` is a filesystem link, relative to the wiki's directory, which names a directory
// if it ends with `/`.
pub const WIKI_EXTENSION: &str = "mull";
pub const TITLE_MARKER: &str = "#";
pub const TITLE_PREFIX: &str = "# ";
pub const FILESYSTEM_LINK_PREFIX: &str = "/";
pub const DIRECTORY_LINK_SUFFIX: &str = "/";

// This title identifies the root of every wiki's text-link graph.
pub const HOME_TITLE: &str = "Home";

// These are the source occurrences through which a node can reference a target.
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
    path: LinkPath,
    is_directory: bool,
}

impl FilesystemTarget {
    // Parse the text of a filesystem link between its delimiters, which starts with `/` and names a
    // directory if it also ends with one. Describe any problem with a message.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = unescape_link_delimiters(text);
        let Some(path) = text.strip_prefix(FILESYSTEM_LINK_PREFIX) else {
            return Err(format!(
                "A filesystem link must start with {}.",
                FILESYSTEM_LINK_PREFIX.code_str(),
            ));
        };
        Self::from_name(path, text.ends_with(DIRECTORY_LINK_SUFFIX))
    }

    // Parse a path written without link syntax or escapes, such as a rename's new name, for a
    // target of the given kind. The path is relative to the wiki directory even if it starts with
    // `/`, and it must stay inside the wiki's logical tree.
    pub fn from_name(path: &str, is_directory: bool) -> Result<Self, String> {
        // Reject components that escape the logical wiki tree [tag:filesystem_path_components]. A
        // root or prefix makes the path absolute.
        let path = Path::new(path.trim_start_matches('/'));
        if path
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
        {
            return Err(format!(
                "Path {} must be relative to the wiki directory.",
                path.code_path(),
            ));
        }

        // A parent component could lead outside the wiki directory.
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(format!(
                "Path {} must not contain {}.",
                path.code_path(),
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

    // Move this target from one directory to another, keeping its kind, if it's the directory or
    // lies within it.
    pub fn moved(&self, from: &LinkPath, to: &LinkPath) -> Option<Self> {
        let suffix = self.path.0.strip_prefix(&from.0).ok()?;
        Some(Self::new(to.0.join(suffix), self.is_directory))
    }

    // Write the link text: `/` and the path's components, followed by `/` for a directory, with any
    // link delimiters escaped. This is the only place that decides how a link is written.
    pub fn text(&self) -> String {
        // Join the components with the separator that links use on every platform. Link paths come
        // from UTF-8 text.
        let components = self
            .path
            .0
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
        escape_link_delimiters(&text)
    }

    // Expose the path relative to the wiki directory.
    pub fn path(&self) -> &LinkPath {
        &self.path
    }

    // Determine whether the target is a directory.
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    // Create a target from a normalized path. The wiki directory is always a directory.
    fn new(path: PathBuf, is_directory: bool) -> Self {
        Self {
            is_directory: is_directory || path.as_os_str().is_empty(),
            path: LinkPath(path),
        }
    }
}

// This is a filesystem link's path relative to the wiki directory, without any root, prefix, `.`,
// or `..` components. It's spelled as written, which may differ from the names on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkPath(PathBuf);

impl LinkPath {
    // Expose the path for display and for resolving it against the wiki directory.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    // Determine whether this path is the wiki directory itself.
    pub fn is_wiki_directory(&self) -> bool {
        self.0.as_os_str().is_empty()
    }
}

// Format a link path for human-facing diagnostic output.
impl CodePath for LinkPath {
    fn code_path(&self) -> ColoredString {
        self.0.code_path()
    }
}

// This struct represents a text node in a wiki.
#[derive(Clone, Debug)]
pub struct TextNode {
    pub title: String,   // Non-empty, one line, trimmed, and not starting with `/`
    pub content: String, // No leading or trailing whitespace, and lines are trimmed at the end
    pub links: Vec<Link>,
    pub depth: Option<usize>, // Minimum text-link distance from the root
    pub source_range: SourceRange, // The complete node without leading or trailing whitespace
    pub title_source_range: SourceRange, // The trimmed title text, not including the `#`
}

// This struct represents a parsed wiki.
#[derive(Clone, Debug, Default)]
pub struct Wiki {
    pub text_nodes: HashMap<String, TextNode>,
}

impl TextNode {
    // Render the node for a Markdown preview without exposing Mull's delimiter escapes, linking
    // each link to the destination the callback provides, if any.
    pub fn to_markdown<F>(&self, mut link_url: F) -> String
    where
        F: FnMut(&Link) -> Option<String>,
    {
        // Render each parsed link with its semantic destination while retaining surrounding prose.
        let mut content = String::new();
        let mut copied_through = 0;
        let mut link_start = None;
        let mut links = self.links.iter();
        let mut previous_was_backslash = false;
        for (index, character) in self.content.char_indices() {
            let is_escaped_delimiter = previous_was_backslash && matches!(character, '[' | ']');
            previous_was_backslash = character == '\\';
            if is_escaped_delimiter {
                continue;
            }

            // Track complete unescaped delimiter pairs, which the parser guarantees are valid.
            match character {
                '[' => link_start = Some(index),
                ']' => {
                    let Some(start) = link_start.take() else {
                        continue;
                    };
                    content.push_str(&render_markdown_prose(&self.content[copied_through..start]));
                    let target = &self.content[start + '['.len_utf8()..index];
                    content.push_str(&match links.next() {
                        Some(link @ Link::Text { .. }) => {
                            render_markdown_text_link(target, link_url(link).as_deref())
                        }
                        Some(link @ Link::Filesystem { .. }) => {
                            render_markdown_filesystem_link(target, link_url(link).as_deref())
                        }
                        None => render_markdown_text_link(target, None),
                    });
                    copied_through = index + character.len_utf8();
                }
                _ => {}
            }
        }
        content.push_str(&render_markdown_prose(&self.content[copied_through..]));

        // Preserve the same title-and-content shape as the Mull rendering without a trailing line.
        let title = render_markdown_literal(&self.title);
        if content.is_empty() {
            format!("{TITLE_PREFIX}{title}")
        } else {
            format!("{TITLE_PREFIX}{title}\n\n{content}")
        }
    }
}

// Render nodes in the wiki's heading-and-content format.
impl fmt::Display for TextNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Omit the content separator when there's no content.
        if self.content.is_empty() {
            writeln!(formatter, "{TITLE_PREFIX}{}", self.title)
        } else {
            writeln!(
                formatter,
                "{TITLE_PREFIX}{}\n\n{}",
                self.title,
                self.content,
            )
        }
    }
}

// Render nodes deterministically in depth order with titles breaking ties.
impl fmt::Display for Wiki {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort reachable nodes by depth and title, followed by unreachable nodes in title order.
        let mut nodes = self.text_nodes.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|(title, node)| (node.depth.is_none(), node.depth, *title));

        // Add one line break between nodes because each node already ends with one.
        for (index, (_title, node)) in nodes.into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            write!(formatter, "{node}")?;
        }

        // Rendering succeeded.
        Ok(())
    }
}

// Escape delimiters so an arbitrary node title or path retains its meaning inside a link.
pub fn escape_link_delimiters(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

// Decode escaped delimiters in link text, as the parser does.
pub fn unescape_link_delimiters(source: &str) -> String {
    source.replace("\\[", "[").replace("\\]", "]")
}

// Hide Mull delimiter escapes in prose while preserving any intentional Markdown formatting.
fn render_markdown_prose(source: &str) -> String {
    source.replace("\\[", "&#91;").replace("\\]", "&#93;")
}

// Render plain text literally in Markdown by replacing its syntax characters with entities. This
// includes `#`, since a trailing sequence of them would otherwise close a heading.
fn render_markdown_literal(text: &str) -> String {
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
    rendered
}

// Render a text link as ordinary bracketed text with an optional Markdown destination.
fn render_markdown_text_link(target: &str, url: Option<&str>) -> String {
    // Keep Markdown punctuation in node titles from changing the rendered label, and retain the
    // visible Mull delimiters inside the clickable region.
    let label = format!(
        "&#91;{}&#93;",
        render_markdown_literal(&unescape_link_delimiters(target)),
    );
    match url {
        Some(url) => format!("[{label}]({url})"),
        None => label,
    }
}

// Render a filesystem link as inline code without exposing Mull delimiter escapes, linking it to an
// optional destination.
fn render_markdown_filesystem_link(target: &str, url: Option<&str>) -> String {
    // Use a fence longer than every backtick run occurring in the link text.
    let source = format!("[{}]", unescape_link_delimiters(target));
    let longest_run = source
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run + 1);

    // The surrounding brackets keep the content distinct from either side of the fence, and angle
    // brackets let the destination contain characters such as parentheses.
    let code = format!("{fence}{source}{fence}");
    match url {
        Some(url) => format!("[{code}](<{url}>)"),
        None => code,
    }
}

#[cfg(test)]
mod tests {
    use super::{FilesystemTarget, Link, TextNode, Wiki};
    use crate::error::SourceRange;
    use std::collections::HashMap;

    // Use a harmless range when testing rendering, which doesn't inspect source locations.
    const SOURCE_RANGE: SourceRange = SourceRange { start: 0, end: 0 };

    // Ensure nodes are rendered in the wiki's source format.
    #[test]
    fn node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: "Hello, world!".to_owned(),
            links: Vec::new(),
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(node.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Ensure empty nodes don't contain a redundant content separator.
    #[test]
    fn empty_node_display() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: String::new(),
            links: Vec::new(),
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(node.to_string(), "# Greeting\n");
    }

    // Hide Mull delimiter escapes while preserving links in Markdown previews.
    #[test]
    fn node_markdown() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: "Literal \\[brackets\\] and [Home].".to_owned(),
            links: vec![Link::Text {
                title: "Home".to_owned(),
                source_range: SOURCE_RANGE,
            }],
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(
            node.to_markdown(|link| match link {
                Link::Text { title, .. } if title == "Home" => {
                    Some("command:mull.revealRange?destination".to_owned())
                }
                Link::Text { .. } | Link::Filesystem { .. } => None,
            }),
            concat!(
                "# Greeting\n\nLiteral &#91;brackets&#93; and ",
                "[&#91;Home&#93;](command:mull.revealRange?destination).",
            ),
        );
    }

    // Render titles literally rather than as Markdown syntax.
    #[test]
    fn node_markdown_title() {
        let node = TextNode {
            title: "A*B* [C](d) <e> #".to_owned(),
            content: String::new(),
            links: Vec::new(),
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(
            node.to_markdown(|_link| None),
            "# A&#42;B&#42; &#91;C&#93;(d) &lt;e&gt; &#35;",
        );
    }

    // Keep unresolved text links visible but non-clickable in Markdown previews.
    #[test]
    fn unresolved_node_markdown_link() {
        let node = TextNode {
            title: "Greeting".to_owned(),
            content: "See [Missing].".to_owned(),
            links: vec![Link::Text {
                title: "Missing".to_owned(),
                source_range: SOURCE_RANGE,
            }],
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(
            node.to_markdown(|_link| None),
            "# Greeting\n\nSee &#91;Missing&#93;.",
        );
    }

    // Distinguish file and directory links from prose with Markdown code styling, linking those
    // with destinations.
    #[test]
    fn filesystem_link_markdown() {
        let node = TextNode {
            title: "Files".to_owned(),
            content: "[/notes.txt] and [/odd`name/]".to_owned(),
            links: vec![
                Link::Filesystem {
                    target: FilesystemTarget::parse("/notes.txt").unwrap(),
                    source_range: SOURCE_RANGE,
                },
                Link::Filesystem {
                    target: FilesystemTarget::parse("/odd`name/").unwrap(),
                    source_range: SOURCE_RANGE,
                },
            ],
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(
            node.to_markdown(|link| match link {
                Link::Filesystem { target, .. } if !target.is_directory() => {
                    Some("file:///wiki/notes.txt".to_owned())
                }
                Link::Text { .. } | Link::Filesystem { .. } => None,
            }),
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
            let target = FilesystemTarget::parse(text).unwrap();
            assert_eq!(target.text(), canonical);
            assert_eq!(target.is_directory(), is_directory);
            assert_eq!(FilesystemTarget::parse(&target.text()).unwrap(), target);
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
        assert_eq!(target.text(), "/photos/a\\[1\\]");
        assert_eq!(
            FilesystemTarget::from_name("photos", true).unwrap().text(),
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
                .moved(directory.path(), destination.path())
                .map(|target| target.text())
        };
        assert_eq!(moved("/photos/"), Some("/archive/photos/".to_owned()));
        assert_eq!(
            moved("/photos/cat.jpg"),
            Some("/archive/photos/cat.jpg".to_owned()),
        );
        assert_eq!(moved("/photographs/cat.jpg"), None);
    }

    // Ensure a wiki containing an empty node has only its trailing line break.
    #[test]
    fn empty_node_wiki_display() {
        let wiki = Wiki {
            text_nodes: HashMap::from([(
                "Greeting".to_owned(),
                TextNode {
                    title: "Greeting".to_owned(),
                    content: String::new(),
                    links: Vec::new(),
                    depth: None,
                    source_range: SOURCE_RANGE,
                    title_source_range: SOURCE_RANGE,
                },
            )]),
        };

        assert_eq!(wiki.to_string(), "# Greeting\n");
    }

    // Render nodes by depth and title, placing nodes without a depth last.
    #[test]
    fn wiki_display() {
        let wiki = Wiki {
            text_nodes: HashMap::from([
                (
                    "Greeting".to_owned(),
                    TextNode {
                        title: "Greeting".to_owned(),
                        content: "Hello, world!".to_owned(),
                        links: Vec::new(),
                        depth: Some(1),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                    },
                ),
                (
                    "Home".to_owned(),
                    TextNode {
                        title: "Home".to_owned(),
                        content: "Check out the [Greeting].".to_owned(),
                        links: Vec::new(),
                        depth: Some(0),
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
                    },
                ),
                (
                    "Orphan".to_owned(),
                    TextNode {
                        title: "Orphan".to_owned(),
                        content: String::new(),
                        links: Vec::new(),
                        depth: None,
                        source_range: SOURCE_RANGE,
                        title_source_range: SOURCE_RANGE,
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
