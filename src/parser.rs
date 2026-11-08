use crate::{
    error::{Error, Fix, SourceRange},
    escaping::{Context, invalid_escape_message, is_escapable, unescape, unescaped_indices},
    format::CodeStr,
    line_index::LineIndex,
    scoring::populate_traversal_order,
    wiki::{
        FILESYSTEM_LINK_PREFIX, FilesystemTarget, Link, MIN_FENCE_LENGTH, MIN_SECTION_LEVEL, Page,
        PageTitle, Portal, SECTION_SEPARATOR, ScriptBlock, Section, SectionTitle, TITLE_MARKER,
        TITLE_PREFIX, Target, Wiki, line_indentation,
    },
};
use std::{collections::HashSet, path::Path};

// Code which starts with a run of at least this many backticks can span lines.
const MIN_MULTILINE_CODE_LENGTH: usize = 3;

// This struct retains what's needed to finish a region at its next boundary: its title, if it has
// a valid one which hasn't been used, where it starts, the links, portals, script blocks, and
// sections in its content, the titles its sections have used, and the sections and portals in the
// copies its portals show.
struct PendingPage {
    title: Option<(PageTitle, SourceRange)>,
    source_start: usize,
    links: Vec<Link>,
    portals: Vec<Portal>,
    script_blocks: Vec<ScriptBlock>,
    sections: Vec<Section>,
    section_titles: HashSet<SectionTitle>,
    copy_sections: Vec<Section>,
    copy_portals: Vec<Portal>,
}

impl PendingPage {
    // Start a region at an offset, with its title if it's a page.
    fn new(title: Option<(PageTitle, SourceRange)>, source_start: usize) -> Self {
        Self {
            title,
            source_start,
            links: Vec::new(),
            portals: Vec::new(),
            script_blocks: Vec::new(),
            sections: Vec::new(),
            section_titles: HashSet::new(),
            copy_sections: Vec::new(),
            copy_portals: Vec::new(),
        }
    }
}

// This struct tracks a script block which has been opened but not yet closed, along with its
// script's name if it's valid, where that is, and where the arguments after it are.
struct OpenBlock {
    fence_length: usize,
    start: usize,                       // The offset of its opening fence's colons
    argument_source_range: SourceRange, // The trimmed text after the colons
    script: Option<(String, SourceRange, SourceRange)>,
    one_line_end: Option<usize>, // The end of the line of a block written on one line
}

// This struct tracks a section or a portal which has been started but not yet ended, outside
// script blocks. Within a portal, it's part of the portal's copy, which belongs to what the portal
// shows.
struct OpenSection {
    kind: OpenSectionKind,
    is_in_copy: bool,
    level: usize,
    start: usize,      // The offset of its first `#`
    body_start: usize, // The offset of the line after its title line
}

// These are the kinds of open sections: a section, whose title is known if it's valid and, outside
// copies, hasn't been used in the page, or a portal.
enum OpenSectionKind {
    Section(Option<(SectionTitle, SourceRange)>),
    Portal(PortalTarget),
}

// This is what a portal shows. It's pending while the portal's title line is read, along with where
// the link which names it should be, and then it's known if that link names a page or a section,
// along with whether it names a section without its page and where the link is.
enum PortalTarget {
    Pending(SourceRange),
    Known(Target, bool, SourceRange),
    Invalid,
}

// These are the kinds of title lines: a section's, with its title if it's valid, or a portal's,
// with where the link which names what it shows should be.
enum TitleLineKind {
    Section(Result<(SectionTitle, SourceRange), Error>),
    Portal(SourceRange),
}

// This struct tracks a link which has been opened but not yet closed, along with the page a section
// named in it without its page would belong to, if there is one.
struct OpenLink {
    start: usize,           // The offset of its opening delimiter
    has_nested_start: bool, // Whether an unexpected opening delimiter appeared in it
    local_title: Option<PageTitle>,
}

// These are the kinds of fence lines. An opening fence's indentation is measured in bytes, and its
// argument is trimmed and located by its offsets within the line. A block written on one line
// ends with a closing run of colons after its argument. A closing fence can be followed by a hash,
// which is located by its offsets within the line, including its `#`.
#[derive(Debug, Eq, PartialEq)]
enum FenceLine {
    Opening {
        indentation: usize,
        length: usize,
        argument: (usize, usize),
        is_one_line: bool,
    },
    Closing {
        length: usize,
        hash: Option<(usize, usize)>,
    },
}

// Parse source contents into a scored wiki in a single pass, with source ranges for every page and
// link, along with any syntax errors in source order. A page's title line starts a region, which is
// a page if its title is valid. In the content, code starts with a run of backticks and ends at the
// next run of exactly as many backticks, on the same line for a run of one or two, and its text is
// taken as written. A link starts at `[` and ends at `]` or the end of its line, and backticks are
// plain characters in it. Its target is split at the first `:` which isn't escaped into a page and
// a section of it, and a link which names a section without its page refers to the page whose
// content it's in. Outside code and the titles of pages and sections, a backslash escapes the next
// character, as allowed by whether it's in a link [ref:content_escapes]. Outside code, a line which
// starts with a run of at least two `#`s and a space, after the indentation of the innermost block
// containing it, is a title line. A title which starts with `[` is a link, and the line starts a
// portal, which shows a copy of the page or section the link names. Otherwise, the line starts a
// section, whose title is taken as written. A line with such a run alone is a closing line. Outside
// script blocks, a title line ends the sections and portals it follows at its level or a deeper
// one, and a closing line ends the section or portal at its level, along with those within it.
// Within a portal, either line is part of the copy. Otherwise, a title line must be one level
// deeper than the section or page it's in, a section's title must be new to the page, and a closing
// line must end a section or a portal. Outside code, a line which starts with a run of colons,
// after any indentation, is a fence. An opening fence starts a script block, named by the script
// which computes its body, followed by the script's arguments. A closing fence consists of colons,
// optionally followed by a hash, and closes the innermost open block with a fence of the same
// length. A script block can also be written on one line, closed by a run of colons after its
// argument, for its script to fill in. The wiki retains as much of the source as possible so it can
// still be validated and navigated, but it omits the content of duplicate pages, invalid titles,
// and anything before the first title, so it mustn't be rendered if there are any syntax errors.
// The line index locates syntax errors in their source listings.
pub fn parse(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> (Wiki, Vec<Error>) {
    parse_at_level(source_path, source_contents, line_index, 1)
}

// Parse source contents as `parse` does, as if each page's title were at a level, so the sections
// in a page start one level deeper, as for a portal's copy read as a page's content at the portal's
// level. The wiki renders its pages at that level.
#[allow(
    clippy::too_many_lines,
    reason = "The function reads the source in one pass, tracking the state of the region, link, \
              blocks, sections, and line being read."
)]
pub fn parse_at_level(
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    title_level: usize,
) -> (Wiki, Vec<Error>) {
    // Accumulate parsed pages, errors, and the state of the scan. The region being read starts as
    // the untitled region before the first title.
    let mut wiki = Wiki::new(source_contents, title_level);
    let mut errors = Vec::<Error>::new();
    let mut pending_page = PendingPage::new(None, 0);
    let mut is_before_first_title = true;
    let mut has_reported_content_before_title = false;
    let mut open_link = None::<OpenLink>;
    let mut open_blocks = Vec::<OpenBlock>::new();
    let mut open_sections = Vec::<OpenSection>::new();
    let mut line_links_before = None::<usize>; // Links before the rest of the line being read
    let mut unclosable_lengths = HashSet::new();
    let mut is_at_line_start = true;
    let mut position = 0;

    // Report a syntax error at a range of the source.
    let syntax_error = |message: &str, error_source_range, fix| {
        Error::new(
            message,
            source_path,
            Some((source_contents, line_index, error_source_range)),
            None,
            fix,
        )
    };

    // Report a link which isn't closed by the end of its line at its opening delimiter, and keep it
    // as a malformed link through the end of its line.
    let close_unclosed_link =
        |links: &mut Vec<Link>, errors: &mut Vec<Error>, link: OpenLink, end: usize| {
            links.push(Link::Malformed {
                source_range: SourceRange {
                    start: link.start,
                    end,
                },
                is_closed: false,
                local_title: link.local_title,
            });
            errors.push(syntax_error(
                "Unclosed link.",
                SourceRange {
                    start: link.start,
                    end: link.start + '['.len_utf8(),
                },
                None,
            ));
        };

    // Once a portal's title line has been read, find the page or section named by the link which
    // makes up its title, which must be the only link read since the title started.
    let read_portal_link =
        |links: &[Link], title_source_range: SourceRange, errors: &mut Vec<Error>| match links {
            [
                Link::Text {
                    target,
                    is_local,
                    source_range,
                },
            ] if *source_range == title_source_range => {
                PortalTarget::Known(target.clone(), *is_local, *source_range)
            }
            [Link::Malformed { .. }] => PortalTarget::Invalid,
            [Link::Filesystem { source_range, .. }] if *source_range == title_source_range => {
                errors.push(syntax_error(
                    "A portal can only show a page or a section.",
                    *source_range,
                    None,
                ));
                PortalTarget::Invalid
            }
            _ => {
                errors.push(syntax_error(
                    &format!(
                        "A portal's title is a single link, like {} or {}.",
                        "## [Page]".code_str(),
                        "## [Page:Section]".code_str(),
                    ),
                    title_source_range,
                    None,
                ));
                PortalTarget::Invalid
            }
        };

    // Report each block which is still open at the end of its page at its opening fence, from its
    // colons through its argument, so the report covers a fence still being typed.
    let report_unclosed_blocks = |open_blocks: &mut Vec<OpenBlock>, errors: &mut Vec<Error>| {
        for open_block in open_blocks.drain(..) {
            errors.push(syntax_error(
                "Unclosed block.",
                SourceRange {
                    start: open_block.start,
                    end: open_block.argument_source_range.end,
                },
                None,
            ));
        }
    };

    // Start a section or a portal at a title line of a level, which starts at an offset, with its
    // body starting at another, after ending the sections and portals it follows at its level or a
    // deeper one. Within a portal, it's part of the copy, which is checked once it's read as the
    // content of what the portal shows. Otherwise, require it to be one level deeper than the
    // section or page it's in, and a section's title to be valid and new to the page.
    let start_section = |open_sections: &mut Vec<OpenSection>,
                         pending_page: &mut PendingPage,
                         errors: &mut Vec<Error>,
                         level: usize,
                         title_line_kind: TitleLineKind,
                         start: usize,
                         body_start: usize| {
        // End the sections and portals it follows, at the start of its line.
        while let Some(open_section) = open_sections.last()
            && open_section.level >= level
        {
            let open_section = open_sections
                .pop()
                .expect("The last open section should exist.");
            finish_section(pending_page, open_section, start, None, source_contents);
        }

        // Outside portals, require the section or portal to be one level deeper than the section
        // or page it's in, which rewriting the `#`s marking its title would fix.
        let is_in_copy = open_sections
            .iter()
            .any(|open_section| matches!(open_section.kind, OpenSectionKind::Portal(_)));
        let expected_level = open_sections
            .last()
            .map_or(title_level, |open_section| open_section.level)
            + 1;
        if !is_in_copy && level != expected_level {
            errors.push(syntax_error(
                &format!(
                    "This title should be marked with {}, one level deeper than the title it's \
                     under.",
                    TITLE_MARKER.to_string().repeat(expected_level).code_str(),
                ),
                SourceRange {
                    start,
                    end: start + level,
                },
                Some(Fix::SetSectionLevel {
                    start,
                    end: start + level,
                    level: expected_level,
                }),
            ));
        }

        // Keep a section's title if it's valid and, outside portals, new to the page. A portal's
        // target is found once the rest of its title line has been read.
        let kind = match title_line_kind {
            TitleLineKind::Section(Ok((title, title_source_range)))
                if !is_in_copy && pending_page.section_titles.contains(&title) =>
            {
                errors.push(syntax_error(
                    &format!("Section {} already exists in this page.", title.code_str()),
                    title_source_range,
                    None,
                ));
                OpenSectionKind::Section(None)
            }
            TitleLineKind::Section(Ok((title, title_source_range))) => {
                if !is_in_copy {
                    pending_page.section_titles.insert(title.clone());
                }
                OpenSectionKind::Section(Some((title, title_source_range)))
            }
            TitleLineKind::Section(Err(error)) => {
                if !is_in_copy {
                    errors.push(error);
                }
                OpenSectionKind::Section(None)
            }
            TitleLineKind::Portal(title_source_range) => {
                OpenSectionKind::Portal(PortalTarget::Pending(title_source_range))
            }
        };
        open_sections.push(OpenSection {
            kind,
            is_in_copy,
            level,
            start,
            body_start,
        });
    };

    // End the section or portal at a closing line's level, which starts at an offset, along with
    // those within it, which end at the start of the line. Within a portal, the line is part of the
    // copy, which is checked once it's read as the content of what the portal shows. Otherwise,
    // there must be a section or a portal at that level to end.
    let close_section = |open_sections: &mut Vec<OpenSection>,
                         pending_page: &mut PendingPage,
                         errors: &mut Vec<Error>,
                         level: usize,
                         line_start: usize| {
        // End the sections and portals within the one at the level, at the start of the line.
        while let Some(open_section) = open_sections.last()
            && open_section.level > level
        {
            let open_section = open_sections
                .pop()
                .expect("The last open section should exist.");
            finish_section(
                pending_page,
                open_section,
                line_start,
                None,
                source_contents,
            );
        }

        // End the section or portal at the level with the line, or report that there isn't one.
        let marker_source_range = SourceRange {
            start: line_start,
            end: line_start + level,
        };
        if let Some(open_section) = open_sections.last()
            && open_section.level == level
        {
            let open_section = open_sections
                .pop()
                .expect("The last open section should exist.");
            finish_section(
                pending_page,
                open_section,
                line_start,
                Some(marker_source_range),
                source_contents,
            );
        } else if !open_sections
            .iter()
            .any(|open_section| matches!(open_section.kind, OpenSectionKind::Portal(_)))
        {
            errors.push(syntax_error(
                &format!(
                    "There's no section marked with {} for this line to end.",
                    TITLE_MARKER.to_string().repeat(level).code_str(),
                ),
                marker_source_range,
                None,
            ));
        }
    };

    // Close the block at an index among those open, reporting any open within it. Keep a script
    // block with a valid name unless it's within a portal or another script block.
    let close_block = |open_blocks: &mut Vec<OpenBlock>,
                       open_sections: &[OpenSection],
                       index: usize,
                       end: usize,
                       hash_source_range: Option<SourceRange>,
                       pending_page: &mut PendingPage,
                       errors: &mut Vec<Error>| {
        let mut inner_blocks = open_blocks.split_off(index);
        let open_block = inner_blocks.remove(0);
        report_unclosed_blocks(&mut inner_blocks, errors);
        if let Some((name, name_source_range, arguments_source_range)) = open_block.script
            && open_blocks.is_empty()
            && !open_sections
                .iter()
                .any(|open_section| matches!(open_section.kind, OpenSectionKind::Portal(_)))
        {
            pending_page.script_blocks.push(ScriptBlock {
                name,
                source_range: SourceRange {
                    start: open_block.start,
                    end,
                },
                name_source_range,
                arguments_source_range,
                hash: hash_source_range.map(|hash_source_range| {
                    source_contents[hash_source_range.start + '#'.len_utf8()..hash_source_range.end]
                        .to_owned()
                }),
            });
        }
    };

    // Once the rest of a portal's title line or an opening fence's line has been read, find what
    // the portal there shows, and close a block written on one line.
    let finish_line = |open_blocks: &mut Vec<OpenBlock>,
                       open_sections: &mut Vec<OpenSection>,
                       pending_page: &mut PendingPage,
                       links_before: usize,
                       errors: &mut Vec<Error>| {
        if let Some(open_section) = open_sections.last_mut()
            && let OpenSectionKind::Portal(PortalTarget::Pending(title_source_range)) =
                open_section.kind
        {
            open_section.kind = OpenSectionKind::Portal(read_portal_link(
                &pending_page.links[links_before..],
                title_source_range,
                errors,
            ));
        }
        if let Some(end) = open_blocks
            .last()
            .and_then(|open_block| open_block.one_line_end)
        {
            close_block(
                open_blocks,
                open_sections,
                open_blocks.len() - 1,
                end,
                None,
                pending_page,
                errors,
            );
        }
    };

    while let Some(character) = source_contents[position..].chars().next() {
        // At the start of a line, which is never within code or a link, read a page's title line,
        // which starts a new region, a section's or a portal's title line, a closing line, or a
        // fence line.
        if is_at_line_start {
            is_at_line_start = false;
            let line_end = source_contents[position..]
                .find('\n')
                .map_or(source_contents.len(), |index| position + index);
            let line = &source_contents[position..line_end];
            let line = line.strip_suffix('\r').unwrap_or(line);
            let line_source_range = SourceRange {
                start: position,
                end: position + line.len(),
            };
            let next_line_start = source_contents.len().min(line_end + '\n'.len_utf8());
            if let Some(raw_title) = title_line_text(line) {
                // Finish the preceding region, then start one which belongs to a page if its title
                // is valid and hasn't been used. Content after a page's title line is in a region.
                is_before_first_title = false;
                report_unclosed_blocks(&mut open_blocks, &mut errors);
                end_sections(
                    &mut open_sections,
                    &mut pending_page,
                    position,
                    source_contents,
                );
                finish_page(&mut wiki, pending_page, position, source_contents);
                let title = match read_name(
                    PageTitle::parse,
                    SourceRange {
                        start: line_source_range.end - raw_title.len(),
                        end: line_source_range.end,
                    },
                    line_source_range.end,
                    line_source_range,
                    source_path,
                    source_contents,
                    line_index,
                ) {
                    Ok((title, title_source_range)) if wiki.pages.contains_key(&title) => {
                        errors.push(syntax_error(
                            &format!("Page {} already exists.", title.code_str()),
                            title_source_range,
                            None,
                        ));
                        None
                    }
                    Ok(title) => Some(title),
                    Err(error) => {
                        errors.push(error);
                        None
                    }
                };
                pending_page = PendingPage::new(title, position);
                position = line_end;
                continue;
            }

            // Read a closing line within a page, which ends the section or portal at its level
            // unless it's within a script block, which takes it as text.
            let indentation = open_blocks.last().map_or("", |open_block| {
                line_indentation(source_contents, open_block.start)
            });
            if !is_before_first_title && let Some(level) = closing_line(line, indentation) {
                if open_blocks.is_empty() {
                    close_section(
                        &mut open_sections,
                        &mut pending_page,
                        &mut errors,
                        level,
                        position,
                    );
                }
                position = line_end;
                continue;
            }

            // Read a section's or a portal's title line within a page, and start it there unless
            // it's within a script block, which takes it as written. A section's title is taken as
            // written, and a portal's is read with the rest of the line for the link which names
            // what it shows.
            if !is_before_first_title
                && let Some((level, title_offset)) = section_title_line(line, indentation)
            {
                if open_blocks.is_empty() {
                    let title_source_range = SourceRange {
                        start: position + title_offset,
                        end: line_source_range.end,
                    };
                    let (kind, link_start) = if is_portal_title(&line[title_offset..]) {
                        let trimmed_title_source_range =
                            trim_source_range(source_contents, title_source_range);
                        (
                            TitleLineKind::Portal(trimmed_title_source_range),
                            Some(trimmed_title_source_range.start),
                        )
                    } else {
                        (
                            TitleLineKind::Section(read_name(
                                SectionTitle::parse,
                                title_source_range,
                                line_source_range.end,
                                line_source_range,
                                source_path,
                                source_contents,
                                line_index,
                            )),
                            None,
                        )
                    };
                    start_section(
                        &mut open_sections,
                        &mut pending_page,
                        &mut errors,
                        level,
                        kind,
                        position,
                        next_line_start,
                    );
                    if let Some(link_start) = link_start {
                        line_links_before = Some(pending_page.links.len());
                        position = link_start;
                        continue;
                    }
                }
                position = line_end;
                continue;
            }

            // Read a fence line within a page. An opening fence starts a script block, which ends
            // with the line if it's written on one line, and the rest of the line is read for the
            // script's name, which must be valid, and its arguments. A closing fence ends the
            // innermost open block with a fence of the same length, and any open within it, noting
            // any hash after it.
            if !is_before_first_title && let Some(fence) = fence_line(line) {
                match fence {
                    FenceLine::Opening {
                        indentation,
                        length,
                        argument: (argument_start, argument_end),
                        is_one_line,
                    } => {
                        let argument = &line[argument_start..argument_end];
                        let argument_source_range = SourceRange {
                            start: position + argument_start,
                            end: position + argument_end,
                        };
                        let name_end = argument.find(char::is_whitespace).unwrap_or(argument.len());
                        let name = &argument[..name_end];
                        let name_source_range = SourceRange {
                            start: argument_source_range.start,
                            end: argument_source_range.start + name_end,
                        };
                        let script = if is_script_name(name) {
                            let script_arguments_start = argument_start + argument.len()
                                - argument[name_end..].trim_start().len();
                            Some((
                                name.to_owned(),
                                name_source_range,
                                SourceRange {
                                    start: position + script_arguments_start,
                                    end: argument_source_range.end,
                                },
                            ))
                        } else {
                            errors.push(syntax_error(
                                &format!(
                                    "A script's name can only contain letters, digits, {}, {}, \
                                     and {}.",
                                    "-".code_str(),
                                    "_".code_str(),
                                    ".".code_str(),
                                ),
                                name_source_range,
                                None,
                            ));
                            None
                        };
                        open_blocks.push(OpenBlock {
                            fence_length: length,
                            start: position + indentation,
                            argument_source_range,
                            script,
                            one_line_end: is_one_line.then_some(position + line.trim_end().len()),
                        });
                        line_links_before = Some(pending_page.links.len());
                        position = argument_source_range.start;
                    }
                    FenceLine::Closing { length, hash } => {
                        if let Some(index) = open_blocks
                            .iter()
                            .rposition(|open_block| open_block.fence_length == length)
                        {
                            close_block(
                                &mut open_blocks,
                                &open_sections,
                                index,
                                line_source_range.end,
                                hash.map(|(start, end)| SourceRange {
                                    start: position + start,
                                    end: position + end,
                                }),
                                &mut pending_page,
                                &mut errors,
                            );
                        } else {
                            errors.push(syntax_error(
                                "Unexpected closing fence.",
                                trim_source_range(source_contents, line_source_range),
                                None,
                            ));
                        }
                        position = line_end;
                    }
                }
                continue;
            }

            // Report only the first non-whitespace content before the first page's title line.
            if is_before_first_title
                && !has_reported_content_before_title
                && !line.trim().is_empty()
            {
                errors.push(syntax_error(
                    "This content isn't in any page.",
                    trim_source_range(source_contents, line_source_range),
                    None,
                ));
                has_reported_content_before_title = true;
            }
        }

        // Read escapes, link delimiters, line breaks, and code.
        match character {
            '\\' => {
                // Skip an escape, or reject a backslash which doesn't escape anything where it
                // is, so it can't be mistaken for an escape, and offer to escape it. The message
                // lists the characters which can be escaped there [ref:content_escapes].
                let context = if open_link.is_some() {
                    Context::Link
                } else {
                    Context::Text
                };
                position += '\\'.len_utf8();
                if let Some(escaped) = source_contents[position..]
                    .chars()
                    .next()
                    .filter(|&escaped| is_escapable(escaped, context))
                {
                    position += escaped.len_utf8();
                } else {
                    let offset = position - '\\'.len_utf8();
                    errors.push(syntax_error(
                        invalid_escape_message(context),
                        SourceRange {
                            start: offset,
                            end: position,
                        },
                        Some(Fix::EscapeBackslash(offset)),
                    ));
                }
                continue;
            }
            '\n' => {
                // End an unclosed link at the end of its line, since links can't contain line
                // breaks, so the following lines are read as usual. Then finish reading a portal's
                // title line or an opening fence on this line.
                if let Some(link) = open_link.take() {
                    let end = if source_contents[..position].ends_with('\r') {
                        position - '\r'.len_utf8()
                    } else {
                        position
                    };
                    close_unclosed_link(&mut pending_page.links, &mut errors, link, end);
                }
                if let Some(links_before) = line_links_before.take() {
                    finish_line(
                        &mut open_blocks,
                        &mut open_sections,
                        &mut pending_page,
                        links_before,
                        &mut errors,
                    );
                }
                is_at_line_start = true;
            }
            '[' => {
                // Open a link, or reject an opening delimiter within one.
                if let Some(link) = &mut open_link {
                    errors.push(syntax_error(
                        "Unexpected opening link delimiter.",
                        SourceRange {
                            start: position,
                            end: position + '['.len_utf8(),
                        },
                        None,
                    ));
                    link.has_nested_start = true;
                } else {
                    open_link = Some(OpenLink {
                        start: position,
                        has_nested_start: false,
                        local_title: local_title(&open_sections, pending_page.title.as_ref()),
                    });
                }
            }
            ']' => {
                // Close the open link, or reject a closing delimiter outside one.
                if let Some(link) = open_link.take() {
                    // Parse the target unless the link has a syntax error, whose target is
                    // unreliable, including a backslash which doesn't escape anything. Keep a
                    // link whose target doesn't parse, or which names a section without its page
                    // where there's no page, as a malformed link.
                    let link_source_range = SourceRange {
                        start: link.start,
                        end: position + ']'.len_utf8(),
                    };
                    let target = source_contents[link.start + '['.len_utf8()..position].trim();
                    let malformed_link = Link::Malformed {
                        source_range: link_source_range,
                        is_closed: true,
                        local_title: link.local_title.clone(),
                    };
                    pending_page.links.push(
                        if link.has_nested_start || unescape(target, Context::Link).is_none() {
                            malformed_link
                        } else {
                            match parse_link(
                                target,
                                link.local_title,
                                source_path,
                                source_contents,
                                line_index,
                                link_source_range,
                            ) {
                                Ok(Some(link)) => link,
                                Ok(None) => malformed_link,
                                Err(error) => {
                                    errors.push(error);
                                    malformed_link
                                }
                            }
                        },
                    );
                } else {
                    errors.push(syntax_error(
                        "Unexpected closing link delimiter.",
                        SourceRange {
                            start: position,
                            end: position + ']'.len_utf8(),
                        },
                        None,
                    ));
                }
            }
            '`' if open_link.is_none() => {
                // Take code as written, skipping to the end of its closing run. A run which isn't
                // closed is reported and otherwise ignored, so it can't hide the rest of the wiki.
                // Once a long run can't be closed, a later run of the same length can't be either,
                // since the text after it was already searched.
                let length = backtick_run_length(&source_contents[position..]);
                let can_close =
                    length < MIN_MULTILINE_CODE_LENGTH || !unclosable_lengths.contains(&length);
                let closing_end = can_close
                    .then(|| closing_run_end(source_contents, position + length, length))
                    .flatten();
                if let Some(end) = closing_end {
                    position = end;
                } else {
                    if length >= MIN_MULTILINE_CODE_LENGTH {
                        unclosable_lengths.insert(length);
                    }
                    errors.push(syntax_error(
                        "Unclosed code.",
                        SourceRange {
                            start: position,
                            end: position + length,
                        },
                        None,
                    ));
                    position += length;
                }
                continue;
            }
            _ => {}
        }
        position += character.len_utf8();
    }

    // Reject a link that's still open at the end of the wiki, finish reading a portal's title line
    // or an opening fence on the last line, and finish the final region.
    if let Some(link) = open_link.take() {
        close_unclosed_link(
            &mut pending_page.links,
            &mut errors,
            link,
            source_contents.len(),
        );
    }
    if let Some(links_before) = line_links_before {
        finish_line(
            &mut open_blocks,
            &mut open_sections,
            &mut pending_page,
            links_before,
            &mut errors,
        );
    }
    report_unclosed_blocks(&mut open_blocks, &mut errors);
    end_sections(
        &mut open_sections,
        &mut pending_page,
        source_contents.len(),
        source_contents,
    );
    finish_page(
        &mut wiki,
        pending_page,
        source_contents.len(),
        source_contents,
    );

    // Order the pages and return the wiki with every error in source order. An unclosed link is
    // only known at the end of its line, after any errors within it.
    populate_traversal_order(&mut wiki);
    errors.sort_by_key(|error| error.source_range().map(|source_range| source_range.start));
    (wiki, errors)
}

// Find the text after the prefix of a page's title line, which is a title marker followed by a
// space. A line that isn't a page's title line has none, including a bare title marker or a
// section's title line.
fn title_line_text(line: &str) -> Option<&str> {
    line.strip_prefix(TITLE_PREFIX)
}

// Read a line as a section's title line written after an indentation: a run of at least two `#`s,
// one for each level, followed by a space and its title. Find its level and where its title starts
// in the line.
fn section_title_line(line: &str, indentation: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix(indentation)?;
    let level = rest.len() - rest.trim_start_matches(TITLE_MARKER).len();
    (level >= MIN_SECTION_LEVEL && rest[level..].starts_with(' '))
        .then_some((level, indentation.len() + level + ' '.len_utf8()))
}

// Read a line as a closing line written after an indentation: a run of at least two `#`s alone, one
// for each level of the section or portal it ends. Find its level.
fn closing_line(line: &str, indentation: &str) -> Option<usize> {
    let rest = line.strip_prefix(indentation)?.trim_end();
    let level = rest.len() - rest.trim_start_matches(TITLE_MARKER).len();
    (level >= MIN_SECTION_LEVEL && level == rest.len()).then_some(level)
}

// Determine whether a title line's title makes it a portal's: one which starts with `[`, opening
// the link which names what the portal shows.
fn is_portal_title(title: &str) -> bool {
    title.trim_start().starts_with('[')
}

// Finish a section or a portal whose content ends at an offset, without any whitespace before it,
// along with its body. A closing line which ends it is part of it too, through its `#`s. Keep a
// section if its title is valid and, outside copies, new to the page, and a portal if its title
// line names what it shows, keeping those in copies apart from the page's own.
fn finish_section(
    pending_page: &mut PendingPage,
    open_section: OpenSection,
    end: usize,
    closing_source_range: Option<SourceRange>,
    source_contents: &str,
) {
    let body_end = open_section.start + source_contents[open_section.start..end].trim_end().len();
    let source_range = SourceRange {
        start: open_section.start,
        end: closing_source_range.map_or(body_end, |closing_source_range| closing_source_range.end),
    };
    let body_source_range = SourceRange {
        start: open_section.body_start.min(body_end),
        end: body_end,
    };
    match open_section.kind {
        OpenSectionKind::Section(Some((title, title_source_range))) => {
            let section = Section {
                title,
                level: open_section.level,
                source_range,
                title_source_range,
                body_source_range,
                closing_source_range,
            };
            if open_section.is_in_copy {
                pending_page.copy_sections.push(section);
            } else {
                pending_page.sections.push(section);
            }
        }
        OpenSectionKind::Portal(PortalTarget::Known(target, is_local, link_source_range)) => {
            let portal = Portal {
                target,
                is_local,
                level: open_section.level,
                source_range,
                link_source_range,
                body_source_range,
                closing_source_range,
            };
            if open_section.is_in_copy {
                pending_page.copy_portals.push(portal);
            } else {
                pending_page.portals.push(portal);
            }
        }
        OpenSectionKind::Section(None)
        | OpenSectionKind::Portal(PortalTarget::Pending(_) | PortalTarget::Invalid) => {}
    }
}

// End the sections and portals still open at the end of a region at an offset.
fn end_sections(
    open_sections: &mut Vec<OpenSection>,
    pending_page: &mut PendingPage,
    end: usize,
    source_contents: &str,
) {
    for open_section in open_sections.drain(..) {
        finish_section(pending_page, open_section, end, None, source_contents);
    }
}

// Add a completed region as a page if it has a valid title which hasn't already been used, with
// the links, blocks, sections, and portals in its content, ordering the sections and portals as
// they're written.
fn finish_page(
    wiki: &mut Wiki,
    pending_page: PendingPage,
    source_end: usize,
    source_contents: &str,
) {
    let PendingPage {
        title,
        source_start,
        links,
        mut portals,
        script_blocks,
        mut sections,
        section_titles: _,
        mut copy_sections,
        mut copy_portals,
    } = pending_page;
    if let Some((title, title_source_range)) = title {
        sections.sort_by_key(|section| section.source_range.start);
        portals.sort_by_key(|portal| portal.source_range.start);
        copy_sections.sort_by_key(|section| section.source_range.start);
        copy_portals.sort_by_key(|portal| portal.source_range.start);
        wiki.pages.insert(
            title.clone(),
            Page {
                title,
                links,
                portals,
                script_blocks,
                sections,
                copy_sections,
                copy_portals,
                traversal_index: None,
                source_range: trim_source_range(
                    source_contents,
                    SourceRange {
                        start: source_start,
                        end: source_end,
                    },
                ),
                title_source_range,
            },
        );
    }
}

// Read a page's or a section's title, the text in a range of the source, rejecting text which
// breaks its rules, such as text which is empty once surrounding whitespace is stripped. Its range
// is extended through any whitespace after it to an offset, so it's found with the cursor anywhere
// after it. A problem is reported at the text, or at the whole line if the text is empty, since it
// then has no text of its own.
fn read_name<T>(
    parse_name: impl FnOnce(&str) -> Result<T, String>,
    text_source_range: SourceRange,
    extended_end: usize,
    line_source_range: SourceRange,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
) -> Result<(T, SourceRange), Error> {
    let trimmed_source_range = trim_source_range(source_contents, text_source_range);
    parse_name(&source_contents[text_source_range.start..text_source_range.end])
        .map(|name| {
            (
                name,
                SourceRange {
                    start: trimmed_source_range.start,
                    end: extended_end,
                },
            )
        })
        .map_err(|message| {
            Error::new(
                &message,
                source_path,
                Some((
                    source_contents,
                    line_index,
                    if trimmed_source_range.start == trimmed_source_range.end {
                        trim_source_range(source_contents, line_source_range)
                    } else {
                        trimmed_source_range
                    },
                )),
                None,
                None,
            )
        })
}

// Read a line as a fence line: optional indentation and a run of fence characters, alone or
// followed by a hash for a closing fence, or followed by an argument for an opening fence, and then
// by another run for a block written on one line. Whitespace between these parts is optional. A
// hash is `#` followed by anything but whitespace. Any other line isn't a fence line.
fn fence_line(line: &str) -> Option<FenceLine> {
    // Measure the indentation and the run after it, which must be long enough.
    let indentation = line.len() - line.trim_start().len();
    let length = line.len() - indentation - line[indentation..].trim_start_matches(':').len();
    if length < MIN_FENCE_LENGTH {
        return None;
    }

    // Find what follows the run, which is nothing, a hash, or an argument.
    let rest = &line[indentation + length..];
    if rest.trim().is_empty() {
        return Some(FenceLine::Closing { length, hash: None });
    }
    let argument_start = line.len() - rest.trim_start().len();
    if rest.trim().starts_with('#') && !rest.trim().contains(char::is_whitespace) {
        return Some(FenceLine::Closing {
            length,
            hash: Some((argument_start, argument_start + rest.trim().len())),
        });
    }
    let argument = line[argument_start..].trim_end();
    let before_closing_run = argument.trim_end_matches(':');
    let is_one_line = argument.len() - before_closing_run.len() >= MIN_FENCE_LENGTH
        && !before_closing_run.trim().is_empty();
    let argument = if is_one_line {
        before_closing_run.trim_end()
    } else {
        argument
    };
    Some(FenceLine::Opening {
        indentation,
        length,
        argument: (argument_start, argument_start + argument.len()),
        is_one_line,
    })
}

// Determine whether a script's name is valid: letters, digits, `-`, `_`, and `.`, which need no
// escaping and can't be confused with the arguments after them.
pub fn is_script_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || "-_.".contains(character))
}

// Find the page a link which names a section without its page refers to: the page shown by the
// innermost portal containing the link, since the copy in it belongs to that page, or else the page
// whose content the link is in. A portal whose title line is being read doesn't contain the link
// in it. There's no such page if the innermost portal doesn't name one or the region isn't a page.
fn local_title(
    open_sections: &[OpenSection],
    title: Option<&(PageTitle, SourceRange)>,
) -> Option<PageTitle> {
    match open_sections
        .iter()
        .rev()
        .find_map(|open_section| match &open_section.kind {
            OpenSectionKind::Portal(PortalTarget::Known(target, _, _)) => Some(Some(&target.title)),
            OpenSectionKind::Portal(PortalTarget::Invalid) => Some(None),
            OpenSectionKind::Portal(PortalTarget::Pending(_)) | OpenSectionKind::Section(_) => None,
        }) {
        Some(title) => title.cloned(),
        None => title.map(|(title, _)| title.clone()),
    }
}

// Convert the contents of a closed delimiter pair, trimmed and with valid escapes, into a typed
// link occurrence. A link which names a section without its page refers to the given page, and
// there's no link if there's none.
fn parse_link(
    target: &str,
    local_title: Option<PageTitle>,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    source_range: SourceRange,
) -> Result<Option<Link>, Error> {
    // Report a problem with the link at the link.
    let link_error = |message: &str| {
        Error::new(
            message,
            source_path,
            Some((source_contents, line_index, source_range)),
            None,
            None,
        )
    };
    let unescape =
        |text: &str| unescape(text, Context::Link).expect("The escapes should be valid.");

    // Parse a filesystem link's target, which starts with `/`.
    if target.starts_with(FILESYSTEM_LINK_PREFIX) {
        return Ok(Some(Link::Filesystem {
            target: FilesystemTarget::parse(&unescape(target))
                .map_err(|message| link_error(&message))?,
            source_range,
        }));
    }
    if target.is_empty() {
        return Err(link_error("This link is missing a target."));
    }

    // Take any other target as the title of the page it links to, followed by the title of a
    // section of it after the first `:` which isn't escaped. A link which names a section without
    // its page refers to the given page.
    let (title, section) = match unescaped_indices(target, SECTION_SEPARATOR, Context::Link).first()
    {
        Some(&index) => (
            &target[..index],
            Some(&target[index + SECTION_SEPARATOR.len_utf8()..]),
        ),
        None => (target, None),
    };
    let section = section
        .map(|section| SectionTitle::parse(&unescape(section)))
        .transpose()
        .map_err(|message| link_error(&message))?;
    let title = unescape(title);
    let is_local = section.is_some() && title.trim().is_empty();
    let title = if is_local {
        let Some(title) = local_title else {
            return Ok(None);
        };
        title
    } else {
        PageTitle::parse(&title).map_err(|message| link_error(&message))?
    };
    Ok(Some(Link::Text {
        target: Target { title, section },
        is_local,
        source_range,
    }))
}

// Count the backticks at the start of some text.
pub fn backtick_run_length(text: &str) -> usize {
    text.len() - text.trim_start_matches('`').len()
}

// Find the end of the run of exactly the given number of backticks which closes code whose text
// starts at an offset, searching only the rest of the line for a run of one or two backticks.
pub fn closing_run_end(source: &str, start: usize, length: usize) -> Option<usize> {
    let end = if length < MIN_MULTILINE_CODE_LENGTH {
        source[start..]
            .find('\n')
            .map_or(source.len(), |index| start + index)
    } else {
        source.len()
    };
    let text = &source[start..end];
    let mut index = 0;
    while let Some(found) = text[index..].find('`') {
        let run_start = index + found;
        let run_length = backtick_run_length(&text[run_start..]);
        if run_length == length {
            return Some(start + run_start + run_length);
        }
        index = run_start + run_length;
    }
    None
}

// Remove surrounding whitespace from a source range without losing its original coordinates.
fn trim_source_range(source_contents: &str, source_range: SourceRange) -> SourceRange {
    // Find the trimmed start relative to the original source.
    let source = &source_contents[source_range.start..source_range.end];
    let start_trimmed = source.trim_start();
    let start = source_range.start + source.len() - start_trimmed.len();

    // Find the trimmed end relative to the adjusted start.
    let trimmed = start_trimmed.trim_end();
    SourceRange {
        start,
        end: start + trimmed.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::{FenceLine, fence_line, parse, parse_at_level, title_line_text};
    use crate::{
        assert_fails,
        error::{Error, Fix, SourceRange},
        escaping::Context,
        line_index::LineIndex,
        wiki::{Link, Page, SECTION_SEPARATOR, Wiki},
    };
    use std::{
        fmt::Write,
        path::{Path, PathBuf},
    };

    // Parse test sources using a stable path for diagnostic assertions, recovering from syntax
    // errors.
    fn parse_fixture(source_contents: &str) -> (Wiki, Vec<Error>) {
        parse(
            Some(Path::new("test.mull")),
            source_contents,
            &LineIndex::new(source_contents),
        )
    }

    // Parse test sources, rejecting sources with syntax errors.
    fn parse_test(source_contents: &str) -> Result<Wiki, Vec<Error>> {
        let (wiki, errors) = parse_fixture(source_contents);
        if errors.is_empty() {
            Ok(wiki)
        } else {
            Err(errors)
        }
    }

    // Describe a page's sections by their titles, levels, whether closing lines end them, and their
    // sources and bodies as written.
    fn sections<'a>(source: &'a str, page: &Page) -> Vec<(String, usize, bool, &'a str, &'a str)> {
        page.sections
            .iter()
            .map(|section| {
                (
                    section.title.text(Context::Verbatim),
                    section.level,
                    section.closing_source_range.is_some(),
                    &source[section.source_range.start..section.source_range.end],
                    &source[section.body_source_range.start..section.body_source_range.end],
                )
            })
            .collect()
    }

    // Describe the targets of the links which parsed without coupling semantic assertions to their
    // source ranges. Malformed links have no targets.
    fn link_targets(links: &[Link]) -> Vec<String> {
        links
            .iter()
            .filter_map(|link| match link {
                Link::Text { target, .. } => Some(format!(
                    "text:{}{}",
                    target.title.text(Context::Verbatim),
                    target.section.as_ref().map_or_else(String::new, |section| {
                        format!("{SECTION_SEPARATOR}{}", section.text(Context::Verbatim))
                    }),
                )),
                Link::Filesystem { target, .. } => Some(format!(
                    "{}:{}",
                    if target.is_directory() { "dir" } else { "file" },
                    target.path().display(),
                )),
                Link::Malformed { .. } => None,
            })
            .collect()
    }

    // Recognize a title marker followed by a space, but not a bare title marker or a section's.
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

    // Parse titles and multiline content while retaining exact source ranges. A title's range
    // extends through trailing whitespace, which the title itself excludes. Rendered content keeps
    // the indentation of its first line.
    #[test]
    fn pages() {
        let source =
            "  \n#   Home  \n\n Check out the [Greeting]. \n\n# Greeting\n Hello,\nworld! \n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(wiki.page("Home").title.text(Context::Verbatim), "Home");
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Greeting"],
        );
        assert_eq!(
            wiki.to_string(),
            "# Home\n\n Check out the [Greeting].\n\n# Greeting\n\n Hello,\nworld!\n",
        );
        assert_eq!(wiki.page("Home").title_source_range.start, 7);
        assert_eq!(wiki.page("Home").title_source_range.end, 13);
        assert_eq!(wiki.page("Home").source_range.start, 3);
        assert_eq!(wiki.page("Home").source_range.end, 41);
        let Link::Text { source_range, .. } = &wiki.page("Home").links[0] else {
            panic!("The parsed link should be a text link.");
        };
        assert_eq!(&source[source_range.start..source_range.end], "[Greeting]");
    }

    // Keep source ranges on UTF-8 boundaries when titles and links contain multibyte characters.
    #[test]
    fn unicode_source_ranges() {
        let source = "# Home\nSee [Grüße].\n# Grüße";
        let wiki = parse_test(source).unwrap();
        let Link::Text { source_range, .. } = &wiki.page("Home").links[0] else {
            panic!("The parsed link should be a text link.");
        };

        assert_eq!(&source[source_range.start..source_range.end], "[Grüße]");
        assert_eq!(
            &source[wiki.page("Grüße").title_source_range.start
                ..wiki.page("Grüße").title_source_range.end],
            "Grüße",
        );
    }

    // Parse Windows line endings without retaining carriage returns.
    #[test]
    fn windows_line_endings() {
        let wiki = parse_test("# Greeting\r\n\r\nHello, world!\r\n").unwrap();

        assert_eq!(wiki.to_string(), "# Greeting\n\nHello, world!\n");
    }

    // Accept empty and whitespace-only wikis.
    #[test]
    fn empty_wiki() {
        assert!(parse_test(" \n\t\n").unwrap().pages.is_empty());
    }

    // Accept empty page content.
    #[test]
    fn empty_content() {
        assert_eq!(parse_test("# Empty").unwrap().to_string(), "# Empty\n");
    }

    // Remove whitespace at the end of each line, including after a link and on blank lines.
    #[test]
    fn trailing_whitespace() {
        let wiki =
            parse_test("# Home\r\nFirst  \t\r\n   \nSee [Home]  \nand [Home] later.\t\nLast")
                .unwrap();

        assert_eq!(
            wiki.to_string(),
            "# Home\n\nFirst\n\nSee [Home]\nand [Home] later.\nLast\n",
        );
    }

    // Keep an indented line which starts with a title marker as content, even as the first line of
    // a page's content, so rendering the wiki doesn't turn it into a title.
    #[test]
    fn indented_title_markers() {
        let source = "# Home\n\n  # Not a title\n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 1);
        assert_eq!(wiki.to_string(), source);
    }

    // Treat hash prefixes which start neither a page's title line nor a section's as ordinary
    // content.
    #[test]
    fn non_title_hashes() {
        let wiki = parse_test("# Home\n\n## Subtitle\n#not a title\n##not a title").unwrap();

        assert_eq!(
            wiki.to_string(),
            "# Home\n\n## Subtitle\n\n#not a title\n##not a title\n",
        );
    }

    // Treat a bare title marker as content, even once formatting removes whitespace after it, so
    // formatting can't turn it into a title.
    #[test]
    fn bare_title_markers() {
        let wiki = parse_test("# Home\n\n#\n#\t\n# \tx").unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(wiki.page("Home").links.len(), 0);
        let formatted = wiki.to_string();
        assert!(formatted.starts_with("# Home\n\n#\n#\n"));
        assert_eq!(parse_test(&formatted).unwrap().pages.len(), 2);
    }

    // Reject titles that are empty after whitespace is stripped.
    #[test]
    fn empty_title() {
        let errors = parse_test("#   \nContent").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A page title can't be empty."),
        );
    }

    // Reject titles that text links would interpret as filesystem links.
    #[test]
    fn filesystem_link_titles() {
        let errors = parse_test("# Home\n\n# /notes.txt\n\n# /\n\n# notes: /draft").unwrap_err();

        assert_eq!(errors.len(), 2);
        for error in &errors {
            assert!(
                error
                    .to_string()
                    .contains("A page title can't start with `/`."),
            );
        }
    }

    // Reject duplicate titles and identify the later declaration.
    #[test]
    fn duplicate_title() {
        let errors = parse_test("# Home\nFirst\n# Home\nSecond").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("Page `Home` already exists."),
        );
        assert!(errors[0].to_string().contains("3 \u{2502} # Home"));
    }

    // Reject non-whitespace content before the first title.
    #[test]
    fn content_before_title() {
        assert_fails!(
            parse_test("Introduction\n# Home"),
            "This content isn't in any page.",
        );
    }

    // Report only the first occurrence of content before a valid title, even when later content
    // looks like a fence or a section's title line.
    #[test]
    fn repeated_content_before_title() {
        let errors = parse_test("First\n:::\n::: sort\n## Second\n# Home").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any page."),
        );
    }

    // Don't reinterpret content after an invalid title as content before the first title.
    #[test]
    fn content_after_empty_title() {
        let errors = parse_test("# Home\n\nfoo\n\n# \n\nbar").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A page title can't be empty."),
        );
    }

    // Recognize opening fences with a trimmed argument, including blocks written on one line, and
    // closing fences, with or without whitespace between their parts, but not shorter runs.
    #[test]
    fn fence_lines() {
        assert_eq!(
            fence_line("::: ## [Page]  "),
            Some(FenceLine::Opening {
                indentation: 0,
                length: 3,
                argument: (4, 13),
                is_one_line: false,
            }),
        );
        assert_eq!(
            fence_line("::::\t## [Page]"),
            Some(FenceLine::Opening {
                indentation: 0,
                length: 4,
                argument: (5, 14),
                is_one_line: false,
            }),
        );
        assert_eq!(
            fence_line(" \t::: ## [Page]"),
            Some(FenceLine::Opening {
                indentation: 2,
                length: 3,
                argument: (6, 15),
                is_one_line: false,
            }),
        );
        assert_eq!(
            fence_line("  :::"),
            Some(FenceLine::Closing {
                length: 3,
                hash: None,
            }),
        );
        assert_eq!(
            fence_line(":::"),
            Some(FenceLine::Closing {
                length: 3,
                hash: None,
            }),
        );
        assert_eq!(
            fence_line("::::: \t"),
            Some(FenceLine::Closing {
                length: 5,
                hash: None,
            }),
        );
        assert_eq!(
            fence_line("  :::: #0a1b2c3d4e5f "),
            Some(FenceLine::Closing {
                length: 4,
                hash: Some((7, 20)),
            }),
        );
        assert_eq!(
            fence_line(":::: sort\t:::::  "),
            Some(FenceLine::Opening {
                indentation: 0,
                length: 4,
                argument: (5, 9),
                is_one_line: true,
            }),
        );
        for (line, argument_end) in [("::: sort ::", 11), ("::: :::", 7)] {
            assert_eq!(
                fence_line(line),
                Some(FenceLine::Opening {
                    indentation: 0,
                    length: 3,
                    argument: (4, argument_end),
                    is_one_line: false,
                }),
            );
        }
        assert_eq!(fence_line(":: ## [Page]"), None);
    }

    // Recognize fence lines without whitespace between their parts: an argument right after the
    // opening run, a closing run right after the argument, and a hash right after a closing run.
    #[test]
    fn fence_lines_without_whitespace() {
        assert_eq!(
            fence_line(" :::sort"),
            Some(FenceLine::Opening {
                indentation: 1,
                length: 3,
                argument: (4, 8),
                is_one_line: false,
            }),
        );
        for line in ["::: sort:::", ":::sort:::"] {
            let argument_start = line.find('s').unwrap();
            assert_eq!(
                fence_line(line),
                Some(FenceLine::Opening {
                    indentation: 0,
                    length: 3,
                    argument: (argument_start, argument_start + "sort".len()),
                    is_one_line: true,
                }),
            );
        }
        assert_eq!(
            fence_line(":::#0a1b"),
            Some(FenceLine::Closing {
                length: 3,
                hash: Some((3, 8)),
            }),
        );
    }

    // Read a portal from its title line through where a section with that title line would end,
    // with the link in its title line among the page's links, and its copy after the title line.
    // The sections and portals in the copy belong to what the portal shows, so they're kept apart
    // from the page's own.
    #[test]
    fn portals() {
        let source = concat!(
            "# Home\n\n## [ Other ]  \n\nSee [Last].\n\n### Notes\n\n### [Last]\n\nMore.\n\n",
            "## After\n\n# Other\n# Last",
        );
        let wiki = parse_test(source).unwrap();
        let home = wiki.page("Home");

        assert_eq!(
            link_targets(&home.links),
            vec!["text:Other", "text:Last", "text:Last"],
        );
        assert_eq!(
            home.portals
                .iter()
                .map(|portal| {
                    (
                        portal.target.title.text(Context::Verbatim),
                        portal.level,
                        portal.closing_source_range.is_some(),
                        &source[portal.source_range.start..portal.source_range.end],
                        &source[portal.link_source_range.start..portal.link_source_range.end],
                        &source[portal.body_source_range.start..portal.body_source_range.end],
                    )
                })
                .collect::<Vec<_>>(),
            vec![(
                "Other".to_owned(),
                2,
                false,
                "## [ Other ]  \n\nSee [Last].\n\n### Notes\n\n### [Last]\n\nMore.",
                "[ Other ]",
                "\nSee [Last].\n\n### Notes\n\n### [Last]\n\nMore.",
            )],
        );
        assert_eq!(
            home.sections
                .iter()
                .map(|section| section.title.text(Context::Verbatim))
                .collect::<Vec<_>>(),
            vec!["After"],
        );
        assert_eq!(
            home.copy_sections
                .iter()
                .map(|section| section.title.text(Context::Verbatim))
                .collect::<Vec<_>>(),
            vec!["Notes"],
        );
        assert_eq!(
            home.copy_portals
                .iter()
                .map(|portal| portal.target.title.text(Context::Verbatim))
                .collect::<Vec<_>>(),
            vec!["Last"],
        );
    }

    // Read a portal which a closing line ends through that line, after which the section containing
    // it continues.
    #[test]
    fn closed_portals() {
        let source = "# Home\n## Notes\n### [Other]\n#### Inner\n###\nMore.\n# Other\n## Inner";
        let wiki = parse_test(source).unwrap();
        let home = wiki.page("Home");

        assert_eq!(
            home.portals
                .iter()
                .map(|portal| {
                    (
                        portal.level,
                        portal.closing_source_range.is_some(),
                        &source[portal.source_range.start..portal.source_range.end],
                        &source[portal.body_source_range.start..portal.body_source_range.end],
                    )
                })
                .collect::<Vec<_>>(),
            vec![(3, true, "### [Other]\n#### Inner\n###", "#### Inner")],
        );
        assert_eq!(
            sections(source, home),
            vec![(
                "Notes".to_owned(),
                2,
                false,
                "## Notes\n### [Other]\n#### Inner\n###\nMore.",
                "### [Other]\n#### Inner\n###\nMore.",
            )],
        );
        assert_eq!(home.copy_sections.len(), 1);
    }

    // Take fence lines as written in code, and take lines which only resemble fences as text.
    #[test]
    fn text_resembling_fences() {
        let wiki = parse_test("# Home\n```\n::: ## [Other]\n```\n:: x\n").unwrap();

        assert!(wiki.page("Home").portals.is_empty());
        assert!(wiki.page("Home").links.is_empty());
    }

    // Reject a portal whose title isn't a single link to a page or a section, but still end it as
    // usual.
    #[test]
    fn invalid_portal_links() {
        assert_fails!(
            parse_test("# Home\n## [Other] more\n# Other"),
            "A portal's title is a single link, like `## [Page]` or `## [Page:Section]`.",
        );
        assert_eq!(
            parse_test("# Home\n## [Other] more\n# Other")
                .unwrap_err()
                .len(),
            1,
        );
        assert_fails!(
            parse_test("# Home\n## [Other] [Last]"),
            "A portal's title is a single link, like `## [Page]` or `## [Page:Section]`.",
        );
        assert_fails!(
            parse_test("# Home\n## [/notes.txt]"),
            "A portal can only show a page or a section.",
        );
    }

    // Read a script block from its opening fence through its closing fence, with its name, the
    // arguments after it, whose links are among the page's links, and any hash after it. Blocks
    // within it aren't kept, since they're part of its body. A script block can also be written on
    // one line.
    #[test]
    fn script_blocks() {
        let source = concat!(
            "# Home\n",
            "  ::: sort -f [Other] `x`\n  - b\n  ::: tidy\n  :::\n  ::: #0a1b\n",
            "::: back-links_2.0 :::\n",
            "# Other",
        );
        let wiki = parse_test(source).unwrap();
        let home = wiki.page("Home");

        assert_eq!(
            home.script_blocks
                .iter()
                .map(|block| {
                    (
                        block.name.as_str(),
                        &source[block.source_range.start..block.source_range.end],
                        &source[block.name_source_range.start..block.name_source_range.end],
                        &source
                            [block.arguments_source_range.start..block.arguments_source_range.end],
                        block.hash.as_deref(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    "sort",
                    "::: sort -f [Other] `x`\n  - b\n  ::: tidy\n  :::\n  ::: #0a1b",
                    "sort",
                    "-f [Other] `x`",
                    Some("0a1b"),
                ),
                (
                    "back-links_2.0",
                    "::: back-links_2.0 :::",
                    "back-links_2.0",
                    "",
                    None,
                ),
            ],
        );
        assert!(home.portals.is_empty());
        assert_eq!(link_targets(&home.links), vec!["text:Other"]);
    }

    // Reject a script's name with characters other than letters, digits, `-`, `_`, and `.`, but
    // still match its closing fence.
    #[test]
    fn invalid_script_names() {
        let errors = parse_test("# Home\n::: so(rt) -f\n:::").unwrap_err();

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("A script's name can only contain letters, digits, `-`, `_`, and `.`."),
        );
        assert!(errors[0].to_string().contains("2 │ ::: so(rt) -f"));
    }

    // Reject closing fences which don't close a block, and blocks which aren't closed by the end of
    // their pages.
    #[test]
    fn invalid_fences() {
        assert_fails!(
            parse_test("# Home\n::: sort\n::::\n:::"),
            "Unexpected closing fence.",
        );
        assert_fails!(parse_test("# Home\n::: sort\n# Other"), "Unclosed block.");
        assert_eq!(
            parse_test("# Home\n:::sort  \n").unwrap_err()[0].source_range(),
            Some(SourceRange { start: 7, end: 14 }),
        );
    }

    // Preserve duplicate links as distinct source occurrences while normalizing their text.
    #[test]
    fn links() {
        let wiki = parse_test(concat!(
            "# Home\nSee [Greeting], [ About ], and [Greeting].",
            "\n# About\n# Greeting",
        ))
        .unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Greeting", "text:About", "text:Greeting"],
        );
        assert_eq!(
            wiki.to_string(),
            "# Home\n\nSee [Greeting], [About], and [Greeting].\n\n# About\n\n# Greeting\n",
        );
    }

    // Parse file and directory links separately from text links.
    #[test]
    fn filesystem_links() {
        let wiki = parse_test(
            "# Home\nSee [/notes.txt], [/images/], [/images/./raw/], [/], [//docs/], and \
                [/ spaced.txt].",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec![
                format!("file:{}", PathBuf::from("notes.txt").display()),
                format!("dir:{}", PathBuf::from("images").display()),
                format!("dir:{}", PathBuf::from("images").join("raw").display()),
                "dir:".to_owned(),
                format!("dir:{}", PathBuf::from("docs").display()),
                format!("file:{}", PathBuf::from(" spaced.txt").display()),
            ],
        );
    }

    // Format links by trimming their targets and normalizing filesystem paths, writing a directory
    // with a trailing `/` and the file root as `/`.
    #[test]
    fn formatted_links() {
        let wiki = parse_test(
            "# Home\n[ Home ] [ /a//b/./c\\[1\\].txt ] [/images/] [/images/./raw//] [/] [/./] \
                [///]",
        )
        .unwrap();

        assert_eq!(
            wiki.to_string(),
            "# Home\n\n[Home] [/a/b/c\\[1\\].txt] [/images/] [/images/raw/] [/] [/] [/]\n",
        );
    }

    // Reject an empty text link, since no page title can be empty, leaving it out of the page's
    // links.
    #[test]
    fn empty_text_links() {
        let (wiki, errors) = parse_fixture("# Home\nSee [] and [  ] and [Home].");

        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("This link is missing a target.")),
        );
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
    }

    // Reject filesystem links whose paths contain parent components.
    #[test]
    fn invalid_filesystem_link_paths() {
        let result = parse_test("# Home\n[/../notes.txt] [/notes/../notes.txt]");

        assert_fails!(result.clone(), "Path `../notes.txt` must not contain `..`.");
        assert_fails!(result, "Path `notes/../notes.txt` must not contain `..`.");
    }

    // Reject opening link delimiters that aren't closed.
    #[test]
    fn unclosed_link() {
        assert_fails!(parse_test("# Home\nSee [Greeting."), "Unclosed link.");
    }

    // End an unclosed link at the end of its line, reading the following lines as usual, so a later
    // closing delimiter is unexpected and the links after it are kept.
    #[test]
    fn unclosed_links_end_at_line_breaks() {
        let (wiki, errors) =
            parse_fixture("# Home\nSee [Greeting\ncontinued]. [Home] and [Other]\n# Other");

        assert_eq!(errors.len(), 2);
        assert!(errors[0].to_string().contains("Unclosed link."));
        assert!(errors[0].to_string().contains("2 \u{2502} See [Greeting"));
        assert!(
            errors[1]
                .to_string()
                .contains("Unexpected closing link delimiter."),
        );
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Home", "text:Other"],
        );
    }

    // Reject unescaped opening delimiters inside links.
    #[test]
    fn unexpected_opening_delimiter() {
        assert_fails!(
            parse_test("# Home\nSee [nested[Greeting]."),
            "Unexpected opening link delimiter.",
        );
    }

    // Reject unescaped closing delimiters outside links.
    #[test]
    fn unexpected_closing_delimiter() {
        let errors = parse_test("# Home\nSee Greeting].").unwrap_err();

        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[0].to_string().contains("2 \u{2502} See Greeting]."));
    }

    // Treat escaped square brackets as literal link-title characters.
    #[test]
    fn escaped_link_delimiters() {
        let wiki = parse_test(
            r"# Home
See \[Ignored\], [One\]Two], [Three\[], [Four], and \[also ignored\].
# Four
# One]Two
# Three[",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:One]Two", "text:Three[", "text:Four"],
        );
    }

    // Treat an escaped backslash as a literal character which doesn't escape what follows. A title
    // is taken as written, so its backslashes need no escaping.
    #[test]
    fn escaped_backslashes() {
        let wiki = parse_test(
            r"# Home
See \\[Four], [Five\\], [A\\B], and \\\[ignored\].
# Four
# Five\
# A\B",
        )
        .unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Four", "text:Five\\", "text:A\\B"],
        );
    }

    // Reject each backslash which doesn't escape anything, including one at the end of a line or a
    // page, offering to escape it, while keeping the page's links.
    #[test]
    fn invalid_escapes() {
        let (wiki, errors) = parse_fixture("# Home\nC:\\Users \\\\ [Other] \\a\\\nend\\\n# Other");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(9)),
                Some(&Fix::EscapeBackslash(27)),
                Some(&Fix::EscapeBackslash(29)),
                Some(&Fix::EscapeBackslash(34)),
            ],
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .contains("A backslash must be followed by")
        }));
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
    }

    // Leave out a link with a backslash which doesn't escape anything, whose target is unreliable,
    // reporting only the backslash.
    #[test]
    fn links_with_invalid_escapes() {
        let (wiki, errors) = parse_fixture("# Home\n[A\\B] [/c\\d.txt] [Other]\n# Other");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(9)),
                Some(&Fix::EscapeBackslash(16)),
            ],
        );
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
    }

    // Reject a backslash before `#`, which can't be escaped, since only title markers at the start
    // of a line are special.
    #[test]
    fn hashes_cant_be_escaped() {
        let (_wiki, errors) = parse_fixture("# Home\nC\\# and \\#1");

        assert_eq!(
            errors.iter().map(Error::fix).collect::<Vec<_>>(),
            vec![
                Some(&Fix::EscapeBackslash(8)),
                Some(&Fix::EscapeBackslash(15)),
            ],
        );
    }

    // Reject an escaped backtick in a link's target, where backticks are plain characters, with a
    // message listing what can be escaped there, and leave out the link. Outside the link, the same
    // escape is accepted.
    #[test]
    fn escaped_backticks_in_links() {
        let (wiki, errors) = parse_fixture("# Home\n\\` [a\\`b] \\` [Home]");

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("In a link, a backslash must be followed by"),
        );
        assert_eq!(errors[0].fix(), Some(&Fix::EscapeBackslash(12)));
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
    }

    // Take the text of code as written, without titles, links, or escapes, whether it's inline or
    // spans lines, while parsing the text around it as usual. Formatting keeps the code's text.
    #[test]
    fn code_spans() {
        let source = concat!(
            "# Home\n\n",
            "Run `[a] C:\\Users`, not ``x ` y``.\n\n",
            "```sh\n# A comment\n[Not a link] C:\\Users \\[ ]\n```\n\n",
            "See [Other].\n\n",
            "# Other\n",
        );
        let wiki = parse_test(source).unwrap();

        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Other"]);
        assert_eq!(wiki.to_string(), source);
    }

    // End code at a closing run of exactly as many backticks, letting code of three or more
    // backticks cross lines, including title lines, and keeping any language name as part of the
    // code. The text after code is read as usual.
    #[test]
    fn code_closing_runs() {
        let wiki = parse_test(concat!(
            "# Home\n",
            "a `[x]` [Home] ``d ` [x]`` [Home]\n",
            "```sh\n# x\n[x] `` ```` \n```\n",
            "[Home]\n",
        ))
        .unwrap();

        assert_eq!(wiki.pages.len(), 1);
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Home", "text:Home", "text:Home"],
        );
    }

    // Treat backticks as plain characters in a title and in a link's target, where formatting
    // leaves them as written, and accept an escaped backtick outside links.
    #[test]
    fn plain_backticks() {
        let source = "# The `ls` command\n\n[The `ls` command] and \\`.\n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(
            link_targets(&wiki.page("The `ls` command").links),
            vec!["text:The `ls` command"],
        );
        assert_eq!(wiki.to_string(), source);
    }

    // Treat backticks as plain characters in a title line, in a link's target, and when escaped,
    // but not elsewhere.
    #[test]
    fn backticks_outside_code() {
        let wiki = parse_test("# A `title\n[A `title] \\`b\\` `[c]`").unwrap();

        assert_eq!(
            link_targets(&wiki.page("A `title").links),
            vec!["text:A `title"],
        );
    }

    // Report a run of backticks which isn't closed, including a short run whose closing run is on
    // a later line, and parse the text after it as usual, so it can't hide the rest of the wiki.
    #[test]
    fn unclosed_code() {
        let (wiki, errors) = parse_fixture("# Home\n```\n[Home] `a\nb`\n# Other");

        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("Unclosed code.")),
        );
        assert!(errors[0].to_string().contains("2 \u{2502} ```"));
        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
    }

    // Report each run of backticks which isn't closed, including a short run whose closing run is
    // on a later line, at the run itself.
    #[test]
    fn unclosed_code_source_ranges() {
        let errors = parse_test("# Home\na `b\nc` ``` d").unwrap_err();

        assert_eq!(
            errors
                .iter()
                .map(|error| error.source_range().unwrap())
                .collect::<Vec<_>>(),
            vec![
                SourceRange { start: 9, end: 10 },
                SourceRange { start: 13, end: 14 },
                SourceRange { start: 15, end: 18 },
            ],
        );
    }

    // Report errors from every invalid page in source order.
    #[test]
    fn multiple_page_errors() {
        let errors = parse_test("# First\nUnexpected].\n# Second\nUnclosed [link.").unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[1].to_string().contains("Unclosed link"));
    }

    // Report every delimiter error within one page in source order, including an unclosed link
    // before the errors within it.
    #[test]
    fn multiple_errors_in_page() {
        let errors = parse_test("# Home\nUnexpected] and [nested[link.").unwrap_err();

        assert_eq!(errors.len(), 3);
        assert!(
            errors[0]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[1].to_string().contains("Unclosed link"));
        assert!(
            errors[2]
                .to_string()
                .contains("Unexpected opening link delimiter"),
        );
    }

    // Report structural and page errors together in source order.
    #[test]
    fn multiple_error_types() {
        let errors = parse_test(concat!(
            "Introduction\n",
            "#   \n",
            "Content\n",
            "# First\n",
            "Unexpected].\n",
            "# Second\n",
            "Unclosed [link.",
        ))
        .unwrap_err();

        assert_eq!(errors.len(), 4);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any page"),
        );
        assert!(
            errors[1]
                .to_string()
                .contains("A page title can't be empty"),
        );
        assert!(
            errors[2]
                .to_string()
                .contains("Unexpected closing link delimiter"),
        );
        assert!(errors[3].to_string().contains("Unclosed link"));
    }

    // Keep a page with a syntax error, along with the targets of the links in it which parsed.
    #[test]
    fn recovered_page_keeps_valid_links() {
        let (wiki, errors) = parse_fixture(
            "# Home\nStray] [Greeting] [Bad\nlink] [nested[link] [/../up] [/notes.txt] [unclosed",
        );

        assert_eq!(errors.len(), 6);
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec![
                "text:Greeting".to_owned(),
                format!("file:{}", PathBuf::from("notes.txt").display()),
            ],
        );
    }

    // Score a wiki with syntax errors so its reachability can be validated.
    #[test]
    fn recovered_wiki_has_traversal_order() {
        let (wiki, errors) = parse_fixture("# Home\nStray] [Greeting]\n# Greeting");

        assert_eq!(errors.len(), 1);
        assert_eq!(wiki.page("Greeting").traversal_index, Some(1));
    }

    // Omit duplicate pages and regions without valid titles, but report the errors in their
    // content.
    #[test]
    fn recovered_wiki_omits_untitled_regions() {
        let (wiki, errors) = parse_fixture(
            "Before]\n# Home\n[Home]\n# Home\n[Greeting] a]\n# \n[Greeting] b]\n# Greeting",
        );

        assert_eq!(errors.len(), 6);
        assert!(
            errors[0]
                .to_string()
                .contains("This content isn't in any page."),
        );
        assert!(errors[1].to_string().contains("1 \u{2502} Before]"));
        assert!(
            errors[2]
                .to_string()
                .contains("Page `Home` already exists."),
        );
        assert!(errors[3].to_string().contains("5 \u{2502} [Greeting] a]"));
        assert!(
            errors[4]
                .to_string()
                .contains("A page title can't be empty."),
        );
        assert!(errors[5].to_string().contains("7 \u{2502} [Greeting] b]"));
        assert_eq!(wiki.pages.len(), 2);
        assert_eq!(link_targets(&wiki.page("Home").links), vec!["text:Home"]);
        assert_eq!(wiki.page("Greeting").traversal_index, None);
    }

    // Keep each link whose target doesn't parse as a malformed link, for an editor to complete:
    // an empty link, one with an invalid escape, one with an unexpected opening delimiter, and one
    // which isn't closed, which extends to the end of its line. What looks like a link in code
    // isn't one.
    #[test]
    fn malformed_links() {
        let (wiki, _errors) = parse_fixture("# Home\na `[b]` [] [c\\`] [d [e] [f");
        let links = &wiki.page("Home").links;

        assert_eq!(
            links
                .iter()
                .map(|link| match link {
                    Link::Malformed {
                        source_range,
                        is_closed,
                        ..
                    } => (source_range.start, source_range.end, *is_closed),
                    Link::Text { .. } | Link::Filesystem { .. } => panic!("The link is malformed."),
                })
                .collect::<Vec<_>>(),
            vec![
                (15, 17, true),
                (18, 23, true),
                (24, 30, true),
                (31, 33, false),
            ],
        );
        assert_eq!(links[0].closing_delimiter(), Some(16));
        assert_eq!(links[3].closing_delimiter(), None);
    }

    // Start a section at each title line, which ends at the next title line of the same or a higher
    // level, at the end of its page, or at a closing line, after which the section containing it
    // continues. Portals and blocks within sections are kept.
    #[test]
    fn sections_of_pages() {
        let source = concat!(
            "# Home\nIntro.\n## Monthly  \nNotes.\n\n### Fixed\nRent.\n#### Utilities\n",
            "Power.\n##### [Other]\n###\nMore.\n### Variable\nFood.\n",
            "## Yearly\n::: sort\n:::\n",
            "# Other",
        );
        let wiki = parse_test(source).unwrap();
        let page = wiki.page("Home");

        assert_eq!(
            sections(source, page),
            vec![
                (
                    "Monthly".to_owned(),
                    2,
                    false,
                    "## Monthly  \nNotes.\n\n### Fixed\nRent.\n#### Utilities\nPower.\n\
                     ##### [Other]\n###\nMore.\n### Variable\nFood.",
                    "Notes.\n\n### Fixed\nRent.\n#### Utilities\nPower.\n##### [Other]\n###\n\
                     More.\n### Variable\nFood.",
                ),
                (
                    "Fixed".to_owned(),
                    3,
                    true,
                    "### Fixed\nRent.\n#### Utilities\nPower.\n##### [Other]\n###",
                    "Rent.\n#### Utilities\nPower.\n##### [Other]",
                ),
                (
                    "Utilities".to_owned(),
                    4,
                    false,
                    "#### Utilities\nPower.\n##### [Other]",
                    "Power.\n##### [Other]",
                ),
                (
                    "Variable".to_owned(),
                    3,
                    false,
                    "### Variable\nFood.",
                    "Food.",
                ),
                (
                    "Yearly".to_owned(),
                    2,
                    false,
                    "## Yearly\n::: sort\n:::",
                    "::: sort\n:::",
                ),
            ],
        );
        assert_eq!(
            &source[page.sections[0].title_source_range.start
                ..page.sections[0].title_source_range.end],
            "Monthly  ",
        );
        assert_eq!(
            &source[page.sections[1].source_range.start..page.sections[1].title_source_range.end],
            "### Fixed",
        );
        assert_eq!(page.portals.len(), 1);
        assert_eq!(page.script_blocks.len(), 1);
    }

    // Require each section to be one level deeper than the title it's under. Sections still nest as
    // their levels say.
    #[test]
    fn section_levels() {
        let (wiki, errors) =
            parse_fixture("# Home\n### Skipped\n## Fine\n#### Deeper\n##\nAfter.\n## Sibling\n");

        assert_eq!(
            errors.iter().map(Error::message).collect::<Vec<_>>(),
            vec![
                "This title should be marked with `##`, one level deeper than the title it's \
                 under.",
                "This title should be marked with `###`, one level deeper than the title it's \
                 under.",
            ],
        );
        assert_eq!(
            errors
                .iter()
                .map(|error| error.source_range().unwrap().start)
                .collect::<Vec<_>>(),
            vec![7, 27],
        );
        assert_eq!(
            wiki.page("Home")
                .sections
                .iter()
                .map(|section| section.title.text(Context::Verbatim))
                .collect::<Vec<_>>(),
            vec!["Skipped", "Fine", "Deeper", "Sibling"],
        );
    }

    // Read the sections of content shown as a section, whose titles start deeper.
    #[test]
    fn deeper_sections() {
        let source = "# Home\n### Deeper\n";
        let (wiki, errors) = parse_at_level(None, source, &LineIndex::new(source), 2);

        assert!(errors.is_empty());
        assert_eq!(wiki.page("Home").sections[0].level, 3);
        assert!(
            !parse_at_level(None, "# Home\n## Shallow", &LineIndex::new(source), 2)
                .1
                .is_empty(),
        );
    }

    // Reject a title used by another section of the same page, keeping the first, but allow it in
    // another page. A title line's `#`s are followed by a space, so other text after them is text.
    #[test]
    fn invalid_section_titles() {
        let (wiki, errors) =
            parse_fixture("# Home\n## Notes\n## Notes\n##\tTab\n# Other\n## Notes");

        assert_eq!(
            errors.iter().map(Error::message).collect::<Vec<_>>(),
            vec!["Section `Notes` already exists in this page."],
        );
        assert_eq!(wiki.page("Home").sections.len(), 1);
        assert_eq!(wiki.page("Other").sections.len(), 1);
    }

    // End a section or a portal at a closing line, which is marked like its title line, along with
    // the sections and portals within it, after which the section or page containing it continues.
    // An indented line isn't a closing line, and neither is a title line.
    #[test]
    fn closing_lines() {
        let source = "# Home\n## A\n### B\n##  \nAfter.\n## C\n  ##\n## ## D\n";
        let wiki = parse_test(source).unwrap();

        assert_eq!(
            sections(source, wiki.page("Home")),
            vec![
                ("A".to_owned(), 2, true, "## A\n### B\n##", "### B"),
                ("B".to_owned(), 3, false, "### B", ""),
                ("C".to_owned(), 2, false, "## C\n  ##", "  ##"),
                ("## D".to_owned(), 2, false, "## ## D", ""),
            ],
        );
    }

    // Reject a closing line which doesn't end a section or a portal, except in a portal's copy,
    // which is checked once it's read as the content of what the portal shows.
    #[test]
    fn invalid_closing_lines() {
        let (_wiki, errors) = parse_fixture("# Home\n##\n## A\n####\n");

        assert_eq!(
            errors.iter().map(Error::message).collect::<Vec<_>>(),
            vec![
                "There's no section marked with `##` for this line to end.",
                "There's no section marked with `####` for this line to end.",
            ],
        );
        assert!(parse_test("# Home\n## [Other]\n####\n# Other").is_ok());
    }

    // Take a title line or a closing line within a script block as written, with no links in it,
    // without starting or ending a section there, and a title line within a portal as part of its
    // copy, where a section's title is still taken as written. A title line within a script block
    // is indented like its opening fence, so a line indented otherwise is text, as is an indented
    // line outside blocks.
    #[test]
    fn title_lines_within_blocks() {
        let source = concat!(
            "# Home\n## [Other]\n### See [Missing]\n##\n::: sort\n## [Missing]\n##\n:::\n",
            "- Item\n  ::: sort\n  ### Inner [Other]\n    ### Text [Other]\n  :::\n",
            "  ## Text [Other]\n",
            "# Other",
        );
        let wiki = parse_test(source).unwrap();
        let page = wiki.page("Home");

        assert!(page.sections.is_empty());
        assert_eq!(
            page.copy_sections
                .iter()
                .map(|section| section.title.text(Context::Verbatim))
                .collect::<Vec<_>>(),
            vec!["See [Missing]"],
        );
        assert_eq!(
            link_targets(&page.links),
            vec!["text:Other", "text:Other", "text:Other"],
        );
    }

    // Split a link's target at the first `:` which isn't escaped into a page and a section, which
    // can be named without its page to refer to the page whose content the link is in, or the page
    // shown by the innermost portal whose copy contains it, until a title line or a closing line
    // ends that portal. A portal's own link is outside its copy. A `#` is a plain character.
    #[test]
    fn section_links() {
        let source = concat!(
            "# Home\n[Other:Notes] [ Other : Notes ] [:Notes] [Time\\: 10\\:30:A\\:B] [C# tips]\n",
            "## [:Notes]\n[:Notes]\n##\n## [Other]\n[:Notes]\n### [Last]\n[:Notes]\n",
            "### Notes\n[:Notes]\n",
            "# Other\n# Time: 10:30\n# C# tips\n# Last",
        );
        let wiki = parse_test(source).unwrap();

        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec![
                "text:Other:Notes",
                "text:Other:Notes",
                "text:Home:Notes",
                "text:Time: 10:30:A:B",
                "text:C# tips",
                "text:Home:Notes",
                "text:Home:Notes",
                "text:Other",
                "text:Other:Notes",
                "text:Last",
                "text:Last:Notes",
                "text:Other:Notes",
            ],
        );
        assert!(matches!(
            wiki.page("Home").links[2],
            Link::Text { is_local: true, .. },
        ));
        assert!(wiki.own_content(wiki.page("Home"), None).starts_with(
            "[Other:Notes] [Other:Notes] [:Notes] [Time\\: 10\\:30:A\\:B] [C# tips]\n",
        ));
    }

    // Reject a page's title which starts with `[`, which would read like a portal's, and a link to
    // a page or a section with such a title, since a section's title line would start a portal.
    #[test]
    fn bracketed_titles() {
        let (_wiki, errors) = parse_fixture("# [Draft]\n# Home\n[\\[Draft\\]] [Home:\\[X\\]]");

        assert_eq!(
            errors.iter().map(Error::message).collect::<Vec<_>>(),
            vec![
                "A page title can't start with `[`.",
                "A page title can't start with `[`.",
                "A section title can't start with `[`.",
            ],
        );
    }

    // Take each `:` after the first one which isn't escaped as part of a section's title, and each
    // `:` in a filesystem link as part of its path, which formatting escapes. Reject a link with an
    // empty section title.
    #[test]
    fn section_link_colons() {
        let (wiki, errors) = parse_fixture("# Home\n[Home:A:B] [Home:] [:] [/a:b] [/a\\:b]");

        assert_eq!(
            errors.iter().map(Error::message).collect::<Vec<_>>(),
            vec![
                "A section title can't be empty.",
                "A section title can't be empty.",
            ],
        );
        assert_eq!(
            link_targets(&wiki.page("Home").links),
            vec!["text:Home:A:B", "file:a:b", "file:a:b"],
        );
        let wiki = parse_test("# Home\n[Home:A:B] [/a:b]").unwrap();
        assert_eq!(
            wiki.own_content(wiki.page("Home"), None),
            "[Home:A\\:B] [/a\\:b]",
        );
    }
}
