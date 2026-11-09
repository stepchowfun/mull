use crate::{
    error::SourceRange,
    escaping::{Context, escape},
    format::CodeStr,
    spelled_path::code_file_path,
};
use colored::ColoredString;
use serde::{Deserialize, Serialize, Serializer};
use std::{
    collections::{HashMap, HashSet},
    fmt, iter,
    path::{Component, Path, PathBuf},
};

// These strings define the wiki format's extension, the suffix that names a wiki's file
// root, and structural markers. A link whose target starts with `/` is a filesystem link,
// relative to the file root, which names a directory if it ends with `/`. Otherwise, it names a
// page, followed by a section of it after `:`. Every title line marks its title with the title
// prefix: a page's at the start of the line, and a section's or a portal's, whose title is a link,
// after spaces. The content under a title is indented the indentation width further than the
// title. The home page's title identifies the root of every wiki's text-link graph.
pub const WIKI_EXTENSION: &str = "mull";
pub const FILE_ROOT_SUFFIX: &str = "_files";
pub const TITLE_PREFIX: &str = "# ";
pub const INDENTATION_WIDTH: usize = 2;
pub const SECTION_SEPARATOR: char = ':';
pub const HOME_TITLE: &str = "Home";
pub const FILESYSTEM_LINK_PREFIX: &str = "/";
pub const DIRECTORY_LINK_SUFFIX: &str = "/";

// A line which starts with at least this many colons, after any indentation, is a fence: an opening
// fence names the script which computes a block's body, and a closing fence consists of colons
// alone or followed by a hash.
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

    // Find the page a target names, along with the section of it, if it names one.
    pub fn resolve(&self, target: &Target) -> Option<(&Page, Option<&Section>)> {
        let page = self.pages.get(&target.title)?;
        match &target.section {
            Some(section_title) => Some((page, Some(page.section(section_title)?))),
            None => Some((page, None)),
        }
    }

    // Find portals which close cycles of portals with a depth-first search over portals, starting
    // from each of the given targets in order. Showing a page or a section means showing the
    // portals within it. A portal to a target whose search is still in progress closes a cycle.
    // Every cycle reachable from the targets has such a portal, and without them there would be no
    // cycles.
    pub fn cycle_closing_portals(&self, targets: impl IntoIterator<Item = Target>) -> Vec<&Portal> {
        let mut cycle_closing_portals = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        for start in targets {
            // Search from a target which exists and hasn't been searched, keeping each target on
            // the current path along with the portals within it which remain to be followed, last
            // first.
            let Some((start_page, start_section)) = self.resolve(&start) else {
                continue;
            };
            if done.contains(&start) {
                continue;
            }
            let portals = start_page
                .portals_within(start_section)
                .rev()
                .collect::<Vec<_>>();
            in_progress.insert(start.clone());
            let mut path = vec![(start, portals)];
            while let Some((target, portals)) = path.last_mut() {
                // Finish a target once all of its portals have been followed.
                let Some(portal) = portals.pop() else {
                    in_progress.remove(target);
                    done.insert(target.clone());
                    path.pop();
                    continue;
                };

                // Follow the next portal, unless it closes a cycle or leads to a target which has
                // been searched or doesn't exist.
                if in_progress.contains(&portal.target) {
                    cycle_closing_portals.push(portal);
                } else if !done.contains(&portal.target)
                    && let Some((shown_page, shown_section)) = self.resolve(&portal.target)
                {
                    in_progress.insert(portal.target.clone());
                    path.push((
                        portal.target.clone(),
                        shown_page.portals_within(shown_section).rev().collect(),
                    ));
                }
            }
        }
        cycle_closing_portals
    }

    // Render a page's content in its canonical form, indented as a page's content is, with each
    // portal refreshed.
    pub fn page_content(&self, page: &Page) -> String {
        self.rendered_content(page, None, INDENTATION_WIDTH, &mut Vec::new())
    }

    // Render the content of a page or a section of it from the source in its canonical form,
    // indented by a number of spaces, with each portal refreshed. The targets being rendered, which
    // show this one in portals, are listed so a cycle of portals is left as written.
    fn rendered_content(
        &self,
        page: &Page,
        section: Option<&Section>,
        indentation: usize,
        enclosing_targets: &mut Vec<Target>,
    ) -> String {
        enclosing_targets.push(page.target(section));
        let content = self.canonical_content(
            page,
            section,
            indentation,
            &HashMap::new(),
            &mut |portal, portal_indentation| {
                if enclosing_targets.contains(&portal.target) {
                    return None;
                }
                let (shown_page, shown_section) = self.resolve(&portal.target)?;
                Some(self.rendered_content(
                    shown_page,
                    shown_section,
                    portal_indentation,
                    enclosing_targets,
                ))
            },
        );
        enclosing_targets.pop();
        content
    }

    // Render the own content of a page or a section of it in its canonical form, leaving out the
    // copies its portals show, which belong to what they show, and without the indentation of its
    // content. Instances of a page or a section with the same own content differ only in ways
    // formatting erases.
    pub fn own_content(&self, page: &Page, section: Option<&Section>) -> String {
        self.own_content_with(page, section, &HashMap::new())
    }

    // Render the own content of a page or a section of it as if each section given content had
    // that content instead, which is also without the indentation of its content.
    pub fn own_content_with(
        &self,
        page: &Page,
        section: Option<&Section>,
        section_contents: &HashMap<SectionTitle, String>,
    ) -> String {
        self.canonical_content(page, section, 0, section_contents, &mut |_portal, _| {
            Some(String::new())
        })
    }

    // Render the content of a page or a section of it from the source, from after its title line
    // but without the blank lines around it, indented by a number of spaces rather than as it is in
    // the source. Each section given content, without the indentation of its content, is written
    // with that content instead. Links, title lines, and fences are written in their canonical
    // forms, with Unix line endings, no whitespace at the end of a line, and a blank line around
    // each title line. Each portal shows the copy given for it at the indentation of its content
    // in the source, or is copied as written if it has none.
    fn canonical_content(
        &self,
        page: &Page,
        section: Option<&Section>,
        indentation: usize,
        section_contents: &HashMap<SectionTitle, String>,
        body: &mut dyn FnMut(&Portal, usize) -> Option<String>,
    ) -> String {
        // Use the content given for the section, if any.
        if let Some(content) = section.and_then(|section| section_contents.get(&section.title)) {
            return reindent(content, 0, indentation);
        }

        // Find the content after the title line, skipping the blank lines around it but keeping
        // the indentation of its first line. A page's title line extends through any whitespace
        // after its title, which can be past the end of a page without content.
        let (start, end, content_indentation) = match section {
            Some(section) => (
                section.body_start,
                section.source_range.end,
                line_indentation(&self.source, section.source_range.start).len()
                    + INDENTATION_WIDTH,
            ),
            None => (
                page.title_source_range.end.min(page.source_range.end),
                page.source_range.end,
                INDENTATION_WIDTH,
            ),
        };
        let text = &self.source[start..end];
        let Some(first_character) = text.find(|character: char| !character.is_whitespace()) else {
            return String::new();
        };
        let start = start
            + text[..first_character]
                .rfind('\n')
                .map_or(0, |index| index + '\n'.len_utf8());
        let end = start + self.source[start..end].trim_end().len();

        // Write the content as it's indented in the source, then move it to the indentation. Links
        // can't contain line breaks, and each ends with a delimiter, so removing the whitespace at
        // the end of each line never changes one.
        let mut content = String::new();
        self.write_region(page, start, end, section_contents, body, &mut content);
        reindent(&content, content_indentation, indentation)
    }

    // Write a region of a page's content, interleaving its portals, script blocks, and sections in
    // source order. What's within a section is written with it. A section can contain portals and
    // script blocks, but nothing is ever within a portal or a script block, since what's within
    // those is part of their text.
    fn write_region(
        &self,
        page: &Page,
        start: usize,
        end: usize,
        section_contents: &HashMap<SectionTitle, String>,
        body: &mut dyn FnMut(&Portal, usize) -> Option<String>,
        content: &mut String,
    ) {
        // List what starts in the region in source order.
        let is_within = |source_range: &SourceRange| (start..end).contains(&source_range.start);
        let mut parts = page
            .portals
            .iter()
            .filter(|portal| is_within(&portal.source_range))
            .map(|portal| (portal.source_range, Part::Portal(portal)))
            .chain(
                page.script_blocks
                    .iter()
                    .filter(|script_block| is_within(&script_block.source_range))
                    .map(|script_block| (script_block.source_range, Part::Script(script_block))),
            )
            .chain(
                page.sections
                    .iter()
                    .filter(|section| is_within(&section.source_range))
                    .map(|section| (section.source_range, Part::Section(section))),
            )
            .collect::<Vec<_>>();
        parts.sort_by_key(|(source_range, _part)| source_range.start);

        // Copy the region, writing each portal with its body, each script block with its fences in
        // their canonical form, within a fence longer than any in it, and each section or portal
        // after a blank line, with its body indented under its title as in the source. What
        // follows a section or a portal comes after a blank line too.
        let mut copied_through = start;
        let mut follows_section = false;
        for (source_range, part) in parts {
            // Skip what's within a section which has been written.
            if source_range.start < copied_through {
                continue;
            }

            // Leave a portal without a body to be copied as written.
            let indentation = line_indentation(&self.source, source_range.start);
            let body_indentation = indentation.len() + INDENTATION_WIDTH;
            let portal_body = match part {
                Part::Portal(portal) => {
                    let Some(portal_body) = body(portal, body_indentation) else {
                        continue;
                    };
                    Some(portal_body)
                }
                Part::Script(_) | Part::Section(_) => None,
            };

            // Copy what precedes the part.
            if !follows_section {
                self.copy_text(page, copied_through, source_range.start, content);
            } else if !self.copy_after_section(page, copied_through, source_range.start, content) {
                separate(content);
                content.push_str(indentation);
            }

            // Write the part.
            follows_section = match part {
                Part::Portal(portal) => {
                    write_titled(
                        content,
                        indentation,
                        &portal.title_line(),
                        &portal_body.expect("A portal's body should have been found."),
                    );
                    true
                }
                Part::Script(script_block) => {
                    self.copy_script_block(page, script_block, content);
                    false
                }
                Part::Section(section) => {
                    write_titled(
                        content,
                        indentation,
                        &section.title_line(),
                        &self.canonical_content(
                            page,
                            Some(section),
                            body_indentation,
                            section_contents,
                            body,
                        ),
                    );
                    true
                }
            };
            copied_through = source_range.end;
        }

        // Copy the rest of the region.
        if follows_section {
            self.copy_after_section(page, copied_through, end, content);
        } else if copied_through < end {
            self.copy_text(page, copied_through, end, content);
        }
    }

    // Copy part of a page's content which follows a section or a portal after a blank line, from
    // the start of its first line which isn't blank. Report whether there was anything but
    // whitespace to copy.
    fn copy_after_section(
        &self,
        page: &Page,
        start: usize,
        end: usize,
        content: &mut String,
    ) -> bool {
        let text = &self.source[start..end];
        let Some(first_character) = text.find(|character: char| !character.is_whitespace()) else {
            return false;
        };
        separate(content);
        self.copy_text(
            page,
            start
                + text[..first_character]
                    .rfind('\n')
                    .map_or(0, |index| index + '\n'.len_utf8()),
            end,
            content,
        );
        true
    }

    // Copy a script block with its fences in their canonical form, keeping its body as written
    // apart from its links. A block written on one line stays on one line.
    fn copy_script_block(&self, page: &Page, script_block: &ScriptBlock, content: &mut String) {
        // Find the lines which start and end the block's body, writing a block written on one line,
        // which has no body, with the fences for an empty body.
        let SourceRange { start, end } = script_block.source_range;
        let block_source = &self.source[start..end];
        let argument = script_block.canonical_argument(&self.source);
        let (Some(opening_end), Some(closing_start)) =
            (block_source.find('\n'), block_source.rfind('\n'))
        else {
            let fence = fence_around("");
            for part in [&fence, " ", &argument, " ", &fence] {
                content.push_str(part);
            }
            return;
        };

        // Write the opening fence, the body, and the closing fence with any hash after it.
        let body_start = start + opening_end + '\n'.len_utf8();
        let body_end = start + closing_start + '\n'.len_utf8();
        let fence = fence_around(&self.source[body_start..body_end]);
        for part in [&fence, " ", &argument, "\n"] {
            content.push_str(part);
        }
        self.copy_text(page, body_start, body_end, content);
        content.push_str(line_indentation(&self.source, start));
        content.push_str(&fence);
        if let Some(hash) = &script_block.hash {
            content.push_str(" #");
            content.push_str(hash);
        }
    }

    // Copy part of a page's content, replacing each link within it with its canonical form: a text
    // link's target escaped, and a filesystem link's normalized target. A malformed link, which a
    // wiki without syntax errors doesn't have, is copied as written.
    fn copy_text(&self, page: &Page, start: usize, end: usize, content: &mut String) {
        let mut copied_through = start;
        for link in page
            .links
            .iter()
            .filter(|link| start <= link.source_range().start && link.source_range().end <= end)
        {
            let target = match link {
                Link::Text {
                    target, is_local, ..
                } => target.link_text(*is_local),
                Link::Filesystem { target, .. } => target.text(Context::Link),
                Link::Malformed { .. } => continue,
            };
            let source_range = link.source_range();
            content.push_str(&self.source[copied_through..source_range.start]);
            content.push('[');
            content.push_str(&target);
            content.push(']');
            copied_through = source_range.end;
        }
        content.push_str(&self.source[copied_through..end]);
    }

    // Find the body of a block, such as a script block, with the indentation of its opening fence
    // removed from each line, along with where each line of the body starts in it and in the
    // source. A block written on one line has no body.
    pub fn body(&self, block: SourceRange) -> Option<(String, Vec<(usize, usize)>)> {
        let block_source = &self.source[block.start..block.end];
        let body_start = block.start + block_source.find('\n')? + '\n'.len_utf8();
        let body_end = block.start + block_source.rfind('\n')? + '\n'.len_utf8();
        Some(self.unindented_lines(
            body_start,
            body_end,
            line_indentation(&self.source, block.start),
        ))
    }

    // Find the copy a portal shows, with the indentation of the portal's title removed from each
    // line so it's indented like a page's content, along with where each line of the copy starts
    // in it and in the source.
    pub fn copy(&self, portal: &Portal) -> (String, Vec<(usize, usize)>) {
        self.unindented_lines(
            portal.body_start,
            portal.source_range.end,
            line_indentation(&self.source, portal.source_range.start),
        )
    }

    // Find the lines in a range of the source with an indentation removed from each, along with
    // where each line starts in the result and in the source.
    fn unindented_lines(
        &self,
        start: usize,
        end: usize,
        indentation: &str,
    ) -> (String, Vec<(usize, usize)>) {
        let mut text = String::new();
        let mut line_starts = Vec::new();
        let mut line_source_start = start;
        for line in self.source[start..end].split_inclusive('\n') {
            let unindented_line = line
                .strip_prefix(indentation)
                .unwrap_or_else(|| line.trim_start_matches([' ', '\t']));
            line_starts.push((
                text.len(),
                line_source_start + line.len() - unindented_line.len(),
            ));
            text.push_str(unindented_line);
            line_source_start += line.len();
        }
        (text, line_starts)
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

        // Write each page as a title line and its content, omitting the content separator when
        // there's no content. Add one line break between pages because each page already ends with
        // one.
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

// This struct represents a page in a wiki. Its portals, script blocks, and sections exclude those
// within portals and script blocks, but not those within sections. The sections and portals in its
// portals' copies are kept apart, since they belong to what the portals show.
#[derive(Clone, Debug)]
pub struct Page {
    pub title: PageTitle,
    pub links: Vec<Link>,                // In source order
    pub portals: Vec<Portal>,            // In source order
    pub script_blocks: Vec<ScriptBlock>, // In source order
    pub sections: Vec<Section>,          // In source order, each with a distinct title
    pub copy_sections: Vec<Section>,     // In source order
    pub copy_portals: Vec<Portal>,       // In source order
    pub traversal_index: Option<usize>,  // Position in a depth-first traversal from the root
    pub source_range: SourceRange,       // The complete page without leading or trailing whitespace
    pub title_source_range: SourceRange, // From the title text, not the `#`, through the line's end
}

impl Page {
    // Find the section of the page with a title.
    pub fn section(&self, title: &SectionTitle) -> Option<&Section> {
        self.sections.iter().find(|section| section.title == *title)
    }

    // Name the page, or a section of it, as a target.
    pub fn target(&self, section: Option<&Section>) -> Target {
        Target {
            title: self.title.clone(),
            section: section.map(|section| section.title.clone()),
        }
    }

    // List the portals within the page, or within a section of it.
    pub fn portals_within(
        &self,
        section: Option<&Section>,
    ) -> impl DoubleEndedIterator<Item = &Portal> {
        let source_range = section.map_or(self.source_range, |section| section.source_range);
        self.portals.iter().filter(move |portal| {
            (source_range.start..source_range.end).contains(&portal.source_range.start)
        })
    }
}

// This struct represents a section of a page's content, which a title line starts. A page's title
// line starts at the start of its line, and each section's is indented the indentation width more
// than the title of the section or page it's in. A section contains the lines after its title line
// which are blank or indented at least the indentation width more than its title, so it ends at the
// first line which isn't.
#[derive(Clone, Debug)]
pub struct Section {
    pub title: SectionTitle,
    pub source_range: SourceRange, // From the `#` through its content
    pub title_source_range: SourceRange, // From the title through the line's end
    pub body_start: usize,         // The start of the line after its title line, or its end
}

impl Section {
    // Write the section's title line in its canonical form, without its indentation.
    pub fn title_line(&self) -> String {
        format!("{TITLE_PREFIX}{}", self.title.text(Context::Verbatim))
    }
}

// This struct represents a portal in a page's content, which shows a page or a section of one. It's
// a section whose title is a link naming what it shows, and it holds a copy of that content, which
// formatting keeps in sync, along with any portals within it. The copy is indented as deep under
// the portal's title as the original is under its own title. A portal to a section can name it
// without its page, which is then the page whose content the portal is in.
#[derive(Clone, Debug)]
pub struct Portal {
    pub target: Target,
    pub is_local: bool,
    pub source_range: SourceRange,      // From the `#` through its copy
    pub link_source_range: SourceRange, // The link in its title line which names the target
    pub body_start: usize,              // The start of the line after its title line, or its end
}

impl Portal {
    // Write the portal's title line in its canonical form, without its indentation.
    pub fn title_line(&self) -> String {
        format!("{TITLE_PREFIX}[{}]", self.target.link_text(self.is_local))
    }
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

impl ScriptBlock {
    // Write what follows the colons of the block's opening fence in its canonical form: the
    // script's name, then any arguments as written, after a single space.
    pub fn canonical_argument(&self, source: &str) -> String {
        let arguments = &source[self.arguments_source_range.start..self.arguments_source_range.end];
        if arguments.is_empty() {
            self.name.clone()
        } else {
            format!("{} {arguments}", self.name)
        }
    }
}

// This is a part of a page's content which formatting rewrites: a portal, a script block, or a
// section.
enum Part<'a> {
    Portal(&'a Portal),
    Script(&'a ScriptBlock),
    Section(&'a Section),
}

// This is a page's title, which text links name it by: non-empty, one line, trimmed, and not
// starting with `[`, which would make its title line look like a portal's, or with `/`, which would
// make a link to it a filesystem link. It travels to the editor and back as JSON in quick fixes, so
// it's checked again when it's read back.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(try_from = "String")]
pub struct PageTitle(String);

impl PageTitle {
    // Make a title from text such as a title line's or a rename's new name, trimming it and
    // describing any rule it breaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        let title = trimmed_title(text, "page")?;
        if title.starts_with(FILESYSTEM_LINK_PREFIX) {
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

// Trim a page's or a section's title, requiring it to be one line, non-empty, and not to start with
// `[`, which starts a portal's title, and naming the kind of title in any message.
fn trimmed_title<'a>(text: &'a str, kind: &str) -> Result<&'a str, String> {
    if text.contains(['\r', '\n']) {
        return Err(format!("A {kind} title can't contain a line break."));
    }
    let title = text.trim();
    if title.is_empty() {
        Err(format!("A {kind} title can't be empty."))
    } else if title.starts_with('[') {
        Err(format!(
            "A {kind} title can't start with {}.",
            "[".code_str(),
        ))
    } else {
        Ok(title)
    }
}

// This is a section's title, which links name it by within its page: non-empty, one line, trimmed,
// and not starting with `[`, which would make its title line a portal's.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SectionTitle(String);

impl SectionTitle {
    // Make a title from text such as a title line's or a rename's new name, trimming it and
    // describing any rule it breaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        Ok(Self(trimmed_title(text, "section")?.to_owned()))
    }

    // Write the title as it appears in a context, such as escaped in a link or as written in a
    // title line.
    pub fn text(&self, context: Context) -> String {
        escape(&self.0, context)
    }
}

// Format a section's title for a message, as written in its title line.
impl CodeStr for SectionTitle {
    fn code_str(&self) -> ColoredString {
        self.0.code_str()
    }
}

// This is what a text link or a portal points at: a page, or a section of one.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Target {
    pub title: PageTitle,
    pub section: Option<SectionTitle>,
}

impl Target {
    // Write the target as it appears between a link's delimiters, without its page if the link
    // names a section without one.
    pub fn link_text(&self, is_local: bool) -> String {
        let mut text = if is_local {
            String::new()
        } else {
            self.title.text(Context::Link)
        };
        if let Some(section) = &self.section {
            text.push(SECTION_SEPARATOR);
            text.push_str(&section.text(Context::Link));
        }
        text
    }
}

// Format a target for a message, with the titles of its page and any section as written in their
// title lines.
impl CodeStr for Target {
    fn code_str(&self) -> ColoredString {
        match &self.section {
            Some(section) => format!("{}{SECTION_SEPARATOR}{}", self.title.0, section.0).code_str(),
            None => self.title.code_str(),
        }
    }
}

// These are the source occurrences through which a page can reference a target. A malformed link
// has a syntax error, such as a missing closing delimiter, so it references nothing, but an editor
// can still complete it, so it notes the page a section named without its page would belong to, if
// there is one. A link which isn't closed extends to the end of its line. A text link to a section
// can name it without its page, which is then the page whose content the link is in.
#[derive(Clone, Debug)]
pub enum Link {
    Text {
        target: Target,
        is_local: bool,
        source_range: SourceRange,
    },
    Filesystem {
        target: FilesystemTarget,
        source_range: SourceRange,
    },
    Malformed {
        source_range: SourceRange,
        is_closed: bool,
        local_title: Option<PageTitle>,
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

// Find the indentation of the line containing an offset, up to the offset.
pub fn line_indentation(source: &str, offset: usize) -> &str {
    &source[source[..offset]
        .rfind('\n')
        .map_or(0, |index| index + '\n'.len_utf8())..offset]
}

// Write a block with an argument after its opening fence and a body, indenting each line after the
// first like the opening fence, which the caller places, within a fence longer than any in it.
pub fn write_block(indentation: &str, argument: &str, body: &str) -> String {
    let fence = fence_around(body);
    let mut block = format!("{fence} {argument}\n");
    for line in body.lines() {
        block.push_str(indentation);
        block.push_str(line);
        block.push('\n');
    }
    block.push_str(indentation);
    block.push_str(&fence);
    block
}

// End what's been written with a blank line, unless nothing has been written, so what's written
// next is separated from it.
fn separate(content: &mut String) {
    content.truncate(content.trim_end().len());
    if !content.is_empty() {
        content.push_str("\n\n");
    }
}

// Write a section or a portal after a blank line: its title line after its indentation, and any
// body, which is indented already, after another blank line.
fn write_titled(content: &mut String, indentation: &str, title_line: &str, body: &str) {
    separate(content);
    content.push_str(indentation);
    content.push_str(title_line);
    if !body.is_empty() {
        content.push_str("\n\n");
        content.push_str(body);
    }
}

// Count the spaces which indent a line.
pub fn indentation_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

// Move each line of some text from one indentation to another, removing up to a number of spaces
// from its start and adding another number. Blank lines are left empty, the whitespace at the end
// of each line is removed, and the lines are joined with Unix line endings.
pub fn reindent(text: &str, from: usize, to: usize) -> String {
    let mut reindented = String::with_capacity(text.len());
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            reindented.push('\n');
        }
        let line = line.trim_end();
        if !line.is_empty() {
            reindented.extend(iter::repeat_n(' ', to));
            reindented.push_str(&line[indentation_of(line).min(from)..]);
        }
    }
    reindented
}

// Make the fence for a block with a body: longer than any run of colons starting a line of the
// body, so none of them closes the block, and no shorter than the shortest fence.
fn fence_around(body: &str) -> String {
    let longest_run = body
        .lines()
        .map(str::trim_start)
        .map(|line| line.len() - line.trim_start_matches(':').len())
        .max()
        .unwrap_or(0);
    ":".repeat((longest_run + 1).max(MIN_FENCE_LENGTH))
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
            "# Greeting\n\n  Hello, world!\n",
            "# Home\n\n  Check out the [Greeting].",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  Check out the [Greeting].\n\n",
                "# Greeting\n\n  Hello, world!\n\n",
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
    // content separator, even if the page's title line ends with whitespace.
    #[test]
    fn empty_page_wiki_display() {
        assert_eq!(parse_wiki("# Greeting\n\n\n").to_string(), "# Greeting\n");
        assert_eq!(
            parse_wiki("# Home\n  See [A].\n# A   \n").to_string(),
            "# Home\n\n  See [A].\n\n# A\n",
        );
    }

    // Write content as written, apart from links in their canonical forms, blank lines around the
    // content, and whitespace at the ends of lines, keeping the indentation of the first line.
    #[test]
    fn content_display() {
        let wiki = parse_wiki(concat!(
            "# Home\r\n\r\n\r\n    Indented  \r\n",
            "  See [ Other ] and [//images/./raw//].  \r\n\r\n",
            "  \tDone.\r\n\r\n",
            "# Other",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n    Indented\n  See [Other] and [/images/raw/].\n\n  \tDone.\n\n",
                "# Other\n",
            ),
        );
    }

    // Refresh each portal with a copy of the content of the page it shows, including the portals in
    // it, indented under the portal's title. A portal to a missing page or one which would complete
    // a cycle is left as written.
    #[test]
    fn portals_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  # [ Other ]\n    stale\n",
            "  # [Missing]\n    kept\n    # Sub\n",
            "  #   [Home]\n    kept\n",
            "# Other\n\n  See [ Last ].\n\n  # [Last]\n\n  # Notes\n\n",
            "# Last\n\n  The end.\n\n  # Details\n\n    More.",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n",
                "  # [Other]\n\n    See [Last].\n\n    # [Last]\n\n      The end.\n\n",
                "      # Details\n\n        More.\n\n",
                "    # Notes\n\n",
                "  # [Missing]\n    kept\n    # Sub\n",
                "  #   [Home]\n    kept\n\n",
                "# Other\n\n  See [Last].\n\n  # [Last]\n\n    The end.\n\n    # Details\n\n",
                "      More.\n\n",
                "  # Notes\n\n",
                "# Last\n\n  The end.\n\n  # Details\n\n    More.\n",
            ),
        );
    }

    // Write what follows a portal outside its copy, after a blank line, and what follows a section
    // within the copy at the copy's indentation.
    #[test]
    fn content_after_portals_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  # [Other]\n    stale\n  After.\n\n",
            "# Other\n\n  See:\n\n  # More\n    Yes.\n  The end.",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  # [Other]\n\n    See:\n\n    # More\n\n      Yes.\n\n    The end.\n\n",
                "  After.\n\n",
                "# Other\n\n  See:\n\n  # More\n\n    Yes.\n\n  The end.\n",
            ),
        );
    }

    // Write a script block's fences in their canonical form, with a space after the colons, a
    // single space between the script's name and its arguments, and the shortest fence longer than
    // any in the body, keeping the body, apart from its links, and any hash.
    #[test]
    fn script_blocks_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  :::sort\n  - b\n  :::\n\n",
            "  :::    sort    desc\n  - a\n  :::#0a1b2c3d4e5f\n\n",
            "  :::::: tidy\n  ::: sort :::\n  ::::::\n\n",
            "  - List:\n    :::sort:::\n\n    ::::: tidy   x  y\n    See [ Home ].\n    :::::\n",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  ::: sort\n  - b\n  :::\n\n",
                "  ::: sort desc\n  - a\n  ::: #0a1b2c3d4e5f\n\n",
                "  :::: tidy\n  ::: sort :::\n  ::::\n\n",
                "  - List:\n    ::: sort :::\n\n    ::: tidy x  y\n    See [Home].\n    :::\n",
            ),
        );
    }

    // Write each title line in its canonical form, with one blank line between it and what's around
    // it.
    #[test]
    fn sections_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n  #   Monthly  \n    Notes.\n\n\n    #   Fixed\n\n      Rent.\n",
            "      # Utilities\n",
            "        Power.\n\n    Back.\n    # Variable\n\n\n      Food.\n  # Yearly\n  # Empty\n",
            "\n",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  # Monthly\n\n    Notes.\n\n    # Fixed\n\n      Rent.\n\n",
                "      # Utilities\n\n",
                "        Power.\n\n    Back.\n\n    # Variable\n\n      Food.\n\n  # Yearly\n\n",
                "  # Empty\n",
            ),
        );
    }

    // Leave a portal to a missing page, or one which would complete a cycle, as written within a
    // copy, but indented like the rest of the copy.
    #[test]
    fn copies_with_portals_as_written_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  # Breakfast\n\n    # [Pancakes]\n\n",
            "# Pancakes\n\n  Mix.\n\n  # [Missing]\n\n    Kept [ Home ].\n\n    # Notes\n\n",
            "      Kept too.\n\n",
            "    Still kept.\n\n  # [Pancakes]\n\n    Kept.\n",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  # Breakfast\n\n    # [Pancakes]\n\n      Mix.\n\n",
                "      # [Missing]\n\n        Kept [Home].\n\n        # Notes\n\n",
                "          Kept too.\n\n",
                "        Still kept.\n\n      # [Pancakes]\n\n        Kept.\n\n",
                "# Pancakes\n\n  Mix.\n\n  # [Missing]\n\n    Kept [Home].\n\n    # Notes\n\n",
                "      Kept too.\n\n",
                "    Still kept.\n\n  # [Pancakes]\n\n    Kept.\n",
            ),
        );
    }

    // Refresh each portal to a section with a copy of the section's content after its title line,
    // along with the portals in it, indented under the portal's title, which may move it left. A
    // portal to a section which doesn't exist is left as written.
    #[test]
    fn section_portals_display() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  # [Other:Monthly]\n\n  # [Other:Closed]\n\n    stale\n\n",
            "  # [Other:Missing]\n\n    kept\n\n  # [:Notes]\n\n  # Notes\n\n    None.\n\n",
            "# Other\n\n  # Closed\n\n    Item.\n      More.\n\n  Between.\n\n",
            "  # Monthly\n\n    Rent.\n\n    # Fixed\n\n      # [Home:Notes]\n\n",
            "  # Yearly\n\n    # Taxes\n\n      Due.\n\n      # Forms\n\n        Many.\n\n",
            "  # [Other:Taxes]\n",
        ));

        assert_eq!(
            wiki.to_string(),
            concat!(
                "# Home\n\n  # [Other:Monthly]\n\n    Rent.\n\n    # Fixed\n\n",
                "      # [Home:Notes]\n\n        None.\n\n",
                "  # [Other:Closed]\n\n    Item.\n      More.\n\n",
                "  # [Other:Missing]\n\n    kept\n\n  # [:Notes]\n\n    None.\n\n",
                "  # Notes\n\n    None.\n\n",
                "# Other\n\n  # Closed\n\n    Item.\n      More.\n\n  Between.\n\n",
                "  # Monthly\n\n    Rent.\n\n    # Fixed\n\n      # [Home:Notes]\n\n",
                "        None.\n\n",
                "  # Yearly\n\n    # Taxes\n\n      Due.\n\n      # Forms\n\n        Many.\n\n",
                "  # [Other:Taxes]\n\n    Due.\n\n    # Forms\n\n      Many.\n",
            ),
        );
    }

    // Render a page's own content in its canonical form, leaving out the copies its portals show.
    #[test]
    fn own_content() {
        let wiki = parse_wiki(concat!(
            "# Home\n\n  See [ Other ].  \r\n\n",
            "  # [Other]\n    Stale.\n    # [Last]\n    More.\n\n  After.\n\n",
            "  # [Last]\n\n    Stale too.\n",
            "# Other\n# Last",
        ));

        assert_eq!(
            wiki.own_content(&wiki.pages[&PageTitle::parse("Home").unwrap()], None),
            "See [Other].\n\n# [Other]\n\nAfter.\n\n# [Last]",
        );
    }

    // Find a block's body without its indentation, and a portal's copy after its title line without
    // the portal's indentation, with where each line starts in the body or copy and in the source.
    // A line indented less than the opening fence loses its indentation, and a block written on one
    // line has no body.
    #[test]
    fn bodies() {
        let source = concat!(
            "# Home\n    ::: sort\n    First\n\n   Second\n    :::\n  ::: sort :::\n",
            "  # [Other]\n\n    Third\n# Other",
        );
        let wiki = parse_wiki(source);
        let home = &wiki.pages[&PageTitle::parse("Home").unwrap()];

        assert_eq!(
            wiki.body(home.script_blocks[0].source_range),
            Some((
                "First\n\nSecond\n".to_owned(),
                vec![(0, 24), (6, 30), (7, 34)],
            )),
        );
        assert_eq!(&source[24..29], "First");
        assert_eq!(&source[34..40], "Second");
        assert_eq!(wiki.body(home.script_blocks[1].source_range), None);
        assert_eq!(
            wiki.copy(&home.portals[0]),
            ("\n  Third".to_owned(), vec![(0, 76), (1, 79)]),
        );
        assert_eq!(&source[81..86], "Third");
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
