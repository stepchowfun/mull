use crate::error::SourceRange;
use std::{collections::HashMap, fmt, path::PathBuf};

// These strings define the wiki format's extension and structural markers.
pub const WIKI_EXTENSION: &str = "mull";
pub const TITLE_MARKER: &str = "#";
pub const TITLE_PREFIX: &str = "# ";
pub const FILE_LINK_PREFIX: &str = "file:";
pub const DIRECTORY_LINK_PREFIX: &str = "dir:";

// This title identifies the root of every wiki's text-link graph.
pub const HOME_TITLE: &str = "Home";

// These are the source occurrences through which a node can reference a target.
#[derive(Clone, Debug)]
pub enum Link {
    Text {
        title: String,
        source_range: SourceRange,
    },
    File {
        path: PathBuf,
        source_range: SourceRange,
    },
    Directory {
        path: PathBuf,
        source_range: SourceRange,
    },
}

// This struct represents a text node in a wiki.
#[derive(Clone, Debug)]
pub struct TextNode {
    pub title: String, // Non-empty, one line, trimmed, and no `file:` or `dir:` prefix
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
    // Render the node for a Markdown preview without exposing Mull's delimiter escapes.
    pub fn to_markdown<F>(&self, mut text_link_url: F) -> String
    where
        F: FnMut(&str) -> Option<String>,
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
                        Some(Link::Text { title, .. }) => {
                            let url = text_link_url(title);
                            render_markdown_text_link(target, url.as_deref())
                        }
                        Some(Link::File { .. } | Link::Directory { .. }) => {
                            render_markdown_filesystem_link(target)
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
        // Omit the content separator when there is no content.
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

// Render a filesystem link as inline code without exposing Mull delimiter escapes.
fn render_markdown_filesystem_link(target: &str) -> String {
    // Use a fence longer than every backtick run occurring in the link text.
    let source = format!("[{}]", unescape_link_delimiters(target));
    let longest_run = source
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run + 1);

    // The surrounding brackets keep the content distinct from either side of the fence.
    format!("{fence}{source}{fence}")
}

#[cfg(test)]
mod tests {
    use super::{DIRECTORY_LINK_PREFIX, FILE_LINK_PREFIX, Link, TextNode, Wiki};
    use crate::error::SourceRange;
    use std::collections::HashMap;

    // Use a harmless range when testing rendering, which does not inspect source locations.
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

    // Ensure empty nodes do not contain a redundant content separator.
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
            node.to_markdown(|title| {
                (title == "Home").then(|| "command:mull.revealRange?destination".to_owned())
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
            node.to_markdown(|_title| None),
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
            node.to_markdown(|_title| None),
            "# Greeting\n\nSee &#91;Missing&#93;.",
        );
    }

    // Distinguish file and directory links from prose with Markdown code styling.
    #[test]
    fn filesystem_link_markdown() {
        let node = TextNode {
            title: "Files".to_owned(),
            content: format!("[{FILE_LINK_PREFIX}notes.txt] and [{DIRECTORY_LINK_PREFIX}odd`name]"),
            links: vec![
                Link::File {
                    path: "notes.txt".into(),
                    source_range: SOURCE_RANGE,
                },
                Link::Directory {
                    path: "odd`name".into(),
                    source_range: SOURCE_RANGE,
                },
            ],
            depth: None,
            source_range: SOURCE_RANGE,
            title_source_range: SOURCE_RANGE,
        };

        assert_eq!(
            node.to_markdown(|_title| None),
            format!(
                "# Files\n\n`[{FILE_LINK_PREFIX}notes.txt]` and \
                    ``[{DIRECTORY_LINK_PREFIX}odd`name]``",
            ),
        );
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
