use crate::{
    error::SourceRange,
    escaping::{Context, is_escapable},
    wiki::title_line_text,
};
use std::collections::HashSet;

// These are the code spans in a wiki's source. A span starts with a run of backticks and ends at
// the next run of exactly as many backticks. A span of one or two backticks must end on the line
// where it starts, but a span of three or more can span lines. Its text is taken as written,
// without titles, links, or escapes. Backticks are plain characters in a title line and in a link's
// target, and an escaped backtick doesn't start a span. A run which isn't closed doesn't start a
// span.
#[derive(Debug, Default)]
pub struct CodeSpans {
    spans: Vec<SourceRange>, // Each from its opening run through its closing run, in order
    unclosed: Vec<SourceRange>, // Each run which isn't closed, in order
}

impl CodeSpans {
    // Find the code spans in a wiki's source.
    pub fn new(source: &str) -> Self {
        // Scan the source outside spans, tracking whether a link is open, since links can't
        // contain line breaks, and whether a line is starting, since a title line has no spans.
        let mut code_spans = Self::default();
        let mut unclosable_lengths = HashSet::new();
        let mut is_in_link = false;
        let mut is_at_line_start = true;
        let mut position = 0;
        while let Some(character) = source[position..].chars().next() {
            // Skip a title line, whose text is taken as written.
            if is_at_line_start {
                is_at_line_start = false;
                let line_end = source[position..]
                    .find('\n')
                    .map_or(source.len(), |index| position + index);
                let line = &source[position..line_end];
                if title_line_text(line.strip_suffix('\r').unwrap_or(line)).is_some() {
                    position = line_end;
                    continue;
                }
            }

            // Skip escapes, track links, and pair each run of backticks outside a link with the run
            // which closes it. Escapes are recognized as in text even within a link, which makes
            // no difference there, since backticks in a link are plain either way. Once a long run
            // can't be closed, a later run of the same length can't be either, since the text after
            // it was already searched.
            match character {
                '\\' => {
                    position += '\\'.len_utf8();
                    if let Some(escaped) = source[position..]
                        .chars()
                        .next()
                        .filter(|&escaped| is_escapable(escaped, Context::Text))
                    {
                        position += escaped.len_utf8();
                    }
                    continue;
                }
                '\n' => {
                    is_in_link = false;
                    is_at_line_start = true;
                }
                '[' => is_in_link = true,
                ']' => is_in_link = false,
                '`' if !is_in_link => {
                    let length = backtick_run_length(&source[position..]);
                    let closing_end = (length < 3 || !unclosable_lengths.contains(&length))
                        .then(|| closing_run_end(source, position + length, length))
                        .flatten();
                    if let Some(end) = closing_end {
                        code_spans.spans.push(SourceRange {
                            start: position,
                            end,
                        });
                        position = end;
                    } else {
                        if length >= 3 {
                            unclosable_lengths.insert(length);
                        }
                        code_spans.unclosed.push(SourceRange {
                            start: position,
                            end: position + length,
                        });
                        position += length;
                    }
                    continue;
                }
                _ => {}
            }
            position += character.len_utf8();
        }
        code_spans
    }

    // Determine whether a source offset is within a code span, including its backticks.
    pub fn contains(&self, offset: usize) -> bool {
        let index = self.spans.partition_point(|span| span.end <= offset);
        self.spans
            .get(index)
            .is_some_and(|span| span.start <= offset)
    }

    // List the runs of backticks which aren't closed, in source order.
    pub fn unclosed(&self) -> &[SourceRange] {
        &self.unclosed
    }
}

// Count the backticks at the start of some text.
fn backtick_run_length(text: &str) -> usize {
    text.len() - text.trim_start_matches('`').len()
}

// Find the end of the run of exactly the given number of backticks which closes a span whose text
// starts at an offset, searching only the rest of the line for a span of one or two backticks.
fn closing_run_end(source: &str, start: usize, length: usize) -> Option<usize> {
    let end = if length < 3 {
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

#[cfg(test)]
mod tests {
    use super::CodeSpans;
    use crate::error::SourceRange;

    // Find the spans in some source, written as the text each covers.
    fn spans(source: &str) -> Vec<&str> {
        CodeSpans::new(source)
            .spans
            .iter()
            .map(|span| &source[span.start..span.end])
            .collect()
    }

    // Close a span with a run of exactly as many backticks, letting a long span cross lines,
    // including title lines, and keeping any language name as part of the code.
    #[test]
    fn closed_spans() {
        assert_eq!(
            spans("a `b` c ``d ` e`` f\n```sh\n# x\n`` ```` \n```"),
            vec!["`b`", "``d ` e``", "```sh\n# x\n`` ```` \n```"],
        );
    }

    // Report a run which isn't closed, including a short run whose closing run is on a later line,
    // without letting it start a span.
    #[test]
    fn unclosed_spans() {
        let source = "a `b\nc` ``` d";
        let code_spans = CodeSpans::new(source);

        assert_eq!(code_spans.spans, Vec::new());
        assert_eq!(
            code_spans.unclosed(),
            &[
                SourceRange { start: 2, end: 3 },
                SourceRange { start: 6, end: 7 },
                SourceRange { start: 8, end: 11 },
            ],
        );
    }

    // Treat backticks as plain characters in a title line, in a link's target, and when escaped.
    #[test]
    fn plain_backticks() {
        let source = "# A `title\n[a `link] \\`b\\` `c`";
        let code_spans = CodeSpans::new(source);

        assert_eq!(spans(source), vec!["`c`"]);
        assert_eq!(code_spans.unclosed(), &[]);
    }

    // Locate offsets within spans, including their backticks but not what follows them.
    #[test]
    fn contains() {
        let code_spans = CodeSpans::new("a `b` c");

        assert!(!code_spans.contains(1));
        assert!(code_spans.contains(2));
        assert!(code_spans.contains(4));
        assert!(!code_spans.contains(5));
    }
}
