use crate::format::CodePath;
use colored::{Colorize, control::SHOULD_COLORIZE};
use std::{
    cmp::{max, min},
    error, fmt,
    path::{Path, PathBuf},
    rc::Rc,
};

// This is the primary error type we'll be using everywhere.
#[derive(Clone, Debug)]
pub struct Error {
    pub message: String,
    pub source_range: Option<SourceRange>,
    source_path: Option<PathBuf>,
    listing: Option<String>,
    pub reason: Option<Rc<dyn error::Error>>,
}

#[cfg(test)]
impl Error {
    // Construct an unadorned error for contexts that do not need a path or source listing.
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            source_range: None,
            source_path: None,
            listing: None,
            reason: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Render the error header from its structured fields.
        write!(f, "{}", "[Error]".red().bold())?;
        if let Some(path) = &self.source_path {
            write!(f, " {}", format!("[{}]", path.code_path()).magenta())?;
        }
        write!(f, " {}", self.message)?;

        // Include the source listing when one is available and nonempty.
        if let Some(listing) = &self.listing
            && !listing.is_empty()
        {
            write!(f, "\n\n{listing}")?;
        }

        // Include the underlying failure when one is available.
        if let Some(reason) = &self.reason {
            write!(f, "\n\n{} {}", "Reason:".blue().bold(), reason)?;
        }

        Ok(())
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.reason.as_deref()
    }
}

// This function constructs a nicely formatted error.
pub fn throw<T: error::Error + 'static>(
    message: &str,
    source_path: Option<&Path>,
    listing: Option<&str>,
    reason: Option<T>,
) -> Error {
    Error {
        message: message.to_owned(),
        source_range: None,
        source_path: source_path.map(Path::to_owned),
        listing: listing.map(str::to_owned),
        reason: reason.map(|reason| -> Rc<dyn error::Error> { Rc::new(reason) }),
    }
}

// For extra type safety, we introduce a dedicated type for source ranges. Tokens and syntax trees
// can use this type instead of tuples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRange {
    pub start: usize, // Inclusive
    pub end: usize,   // Exclusive
}

// Construct an error that retains the source range used by terminal and editor diagnostics.
pub fn source_error(
    message: &str,
    source_path: &Path,
    source_contents: &str,
    source_range: SourceRange,
) -> Error {
    let mut error = throw::<Error>(
        message,
        Some(source_path),
        Some(&listing(source_contents, source_range)),
        None,
    );
    error.source_range = Some(source_range);
    error
}

// Construct a source-aware error that retains an underlying failure.
pub fn source_error_with_reason<T: error::Error + 'static>(
    message: &str,
    source_path: &Path,
    source_contents: &str,
    source_range: SourceRange,
    reason: T,
) -> Error {
    let mut error = throw(
        message,
        Some(source_path),
        Some(&listing(source_contents, source_range)),
        Some(reason),
    );
    error.source_range = Some(source_range);
    error
}

// This function renders the relevant lines of a source file given the source file contents and a
// range. The range is inclusive on the left and exclusive on the right.
pub fn listing(source_contents: &str, source_range: SourceRange) -> String {
    // Remember the relevant lines and the position of the start of the next line.
    let mut lines = vec![];
    let mut pos = 0_usize;

    // Find the relevant lines.
    for (i, line) in source_contents.split('\n').enumerate() {
        // Record the start of the line before we advance the cursor.
        let line_start = pos;

        // Move the cursor to the start of the next line.
        pos += line.len() + 1;

        // If we're past the lines of interest, we're done.
        if line_start >= source_range.end {
            break;
        }

        // If we haven't reached the lines of interest yet, skip to the next line.
        if pos <= source_range.start {
            continue;
        }

        // We trim the end of the line to remove any carriage return (or any other whitespace) that
        // might have been present before the line feed.
        let trimmed_line = line.trim_end();

        // Highlight the relevant part of the line.
        let (section_start, section_end) = if source_range.start > line_start {
            (
                min(source_range.start - line_start, trimmed_line.len()),
                min(source_range.end - line_start, trimmed_line.len()),
            )
        } else {
            let end = min(source_range.end - line_start, trimmed_line.len());
            let start = trimmed_line
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(end);

            (start, end)
        };

        // Record the line number and the line contents.
        lines.push((
            (i + 1).to_string(),
            trimmed_line,
            section_start,
            section_end,
        ));
    }

    // Compute the width of the string representation of the hugest relevant line number.
    let gutter_width = lines.iter().fold(0_usize, |acc, (line_number, _, _, _)| {
        max(acc, line_number.len())
    });

    // Determine whether the output will be colorized.
    let colorized = SHOULD_COLORIZE.should_colorize();

    // Render the code listing with line numbers.
    lines
        .iter()
        .enumerate()
        .map(|(i, (line_number, line, section_start, section_end))| {
            format!(
                "{}{}{}{}{}",
                format!("{line_number:>gutter_width$} \u{2502} ")
                    .blue()
                    .bold(),
                &line[..*section_start],
                line[*section_start..*section_end].red(),
                &line[*section_end..],
                if colorized {
                    String::new()
                } else if section_start == section_end {
                    format!(
                        "\n{} {}",
                        " ".repeat(gutter_width),
                        if i == lines.len() - 1 {
                            " "
                        } else {
                            "\u{250a}"
                        },
                    )
                } else {
                    format!(
                        "\n{} {} {}{}",
                        " ".repeat(gutter_width),
                        if i == lines.len() - 1 {
                            " "
                        } else {
                            "\u{250a}"
                        },
                        " ".repeat(*section_start),
                        // [tag:overline_u203e]
                        "\u{203e}".repeat(section_end - section_start),
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Format a list while avoiding an extra visual gap after a source-range underline.
pub fn format_errors(errors: &[Error]) -> String {
    errors
        .iter()
        .fold(String::new(), |acc, error| {
            format!(
                "{}\n{}{}",
                acc,
                // Only render an empty line between errors here if the previous line doesn't
                // already visually look like an empty line. See [ref:overline_u203e].
                if acc
                    .split('\n')
                    .next_back()
                    .unwrap() // Safe since `split` always results in at least one item
                    .chars()
                    .all(|c| c == ' ' || c == '\u{203e}')
                {
                    ""
                } else {
                    "\n"
                },
                error,
            )
        })
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use crate::error::{Error, SourceRange, format_errors, listing, source_error, throw};
    use std::{path::Path, rc::Rc};

    #[test]
    fn error_no_reason_display() {
        assert_eq!(
            Error::new("Something went wrong.").to_string(),
            "[Error] Something went wrong.",
        );
    }

    #[test]
    fn error_with_reason_display() {
        let mut error = Error::new("Something went wrong.");
        error.reason = Some(Rc::new(Error::new("Something deeper went wrong.")));

        assert_eq!(
            error.to_string(),
            "\
                [Error] Something went wrong.\n\
                \n\
                Reason: [Error] Something deeper went wrong.\
            ",
        );
    }

    #[test]
    fn throw_no_source_path_listing_reason() {
        let error = throw::<Error>("An error occurred.", None, None, None);

        assert_eq!(error.message, "An error occurred.");
        assert_eq!(error.to_string(), "[Error] An error occurred.");
        assert!(error.source_range.is_none());
    }

    #[test]
    fn throw_with_source_path_no_listing_reason() {
        assert_eq!(
            throw::<Error>("An error occurred.", Some(Path::new("foo")), None, None).to_string(),
            "[Error] [`foo`] An error occurred.",
        );
    }

    #[test]
    fn throw_with_listing_no_source_path_reason() {
        assert_eq!(
            throw::<Error>("An error occurred.", None, Some("It happened here."), None).to_string(),
            "[Error] An error occurred.\n\nIt happened here.",
        );
    }

    #[test]
    fn throw_with_reason_no_source_path_listing() {
        let reason = throw::<Error>("An deeper error occurred.", None, None, None);

        assert_eq!(
            throw::<Error>("An error occurred.", None, None, Some(reason)).to_string(),
            "[Error] An error occurred.\n\nReason: [Error] An deeper error occurred.",
        );
    }

    #[test]
    fn throw_with_source_path_listing_no_reason() {
        assert_eq!(
            throw::<Error>(
                "An error occurred.",
                Some(Path::new("foo")),
                Some("It happened here."),
                None,
            )
            .to_string(),
            "[Error] [`foo`] An error occurred.\n\nIt happened here.",
        );
    }

    #[test]
    fn throw_with_listing_reason_no_source_path() {
        let reason = throw::<Error>("An deeper error occurred.", None, None, None);

        assert_eq!(
            throw::<Error>(
                "An error occurred.",
                None,
                Some("It happened here."),
                Some(reason),
            )
            .to_string(),
            concat!(
                "[Error] An error occurred.\n\nIt happened here.\n\n",
                "Reason: [Error] An deeper error occurred.",
            ),
        );
    }

    #[test]
    fn throw_with_source_path_reason_no_listing() {
        let reason = throw::<Error>("An deeper error occurred.", None, None, None);

        assert_eq!(
            throw::<Error>(
                "An error occurred.",
                Some(Path::new("foo")),
                None,
                Some(reason),
            )
            .to_string(),
            "[Error] [`foo`] An error occurred.\n\nReason: [Error] An deeper error occurred.",
        );
    }

    #[test]
    fn throw_with_source_path_listing_reason() {
        let reason = throw::<Error>("An deeper error occurred.", None, None, None);

        assert_eq!(
            throw::<Error>(
                "An error occurred.",
                Some(Path::new("foo")),
                Some("It happened here."),
                Some(reason),
            )
            .to_string(),
            concat!(
                "[Error] [`foo`] An error occurred.\n\nIt happened here.\n\n",
                "Reason: [Error] An deeper error occurred.",
            ),
        );
    }

    #[test]
    fn source_error_retains_range() {
        let source_range = SourceRange { start: 1, end: 3 };
        let error = source_error("An error occurred.", Path::new("foo"), "abcd", source_range);

        assert_eq!(error.message, "An error occurred.");
        assert_eq!(error.source_range, Some(source_range));
        assert_eq!(
            error.to_string(),
            "[Error] [`foo`] An error occurred.\n\n1 \u{2502} abcd\n     \u{203e}\u{203e}",
        );
    }

    #[test]
    fn listing_empty() {
        assert_eq!(listing("", SourceRange { start: 0, end: 0 }), "");
    }

    #[test]
    fn listing_single_line_full_range() {
        assert_eq!(
            listing("foo bar", SourceRange { start: 0, end: 7 }),
            "1 \u{2502} foo bar\n    \u{203e}\u{203e}\u{203e}\u{203e}\u{203e}\u{203e}\u{203e}",
        );
    }

    #[test]
    fn listing_single_line_partial_range() {
        assert_eq!(
            listing("foo bar", SourceRange { start: 1, end: 6 }),
            "1 \u{2502} foo bar\n     \u{203e}\u{203e}\u{203e}\u{203e}\u{203e}",
        );
    }

    #[test]
    fn listing_multiple_lines_full_range() {
        assert_eq!(
            listing("foo\nbar\nbaz\nqux", SourceRange { start: 0, end: 15 }),
            "1 \u{2502} foo\n  \u{250a} \u{203e}\u{203e}\u{203e}\n2 \u{2502} bar\n  \u{250a} \
                \u{203e}\u{203e}\u{203e}\n3 \u{2502} baz\n  \u{250a} \u{203e}\u{203e}\u{203e}\n4 \
                \u{2502} qux\n    \u{203e}\u{203e}\u{203e}",
        );
    }

    #[test]
    fn listing_multiple_lines_partial_range() {
        assert_eq!(
            listing("foo\nbar\nbaz\nqux", SourceRange { start: 5, end: 9 }),
            "2 \u{2502} bar\n  \u{250a}  \u{203e}\u{203e}\n3 \u{2502} baz\n    \u{203e}",
        );
    }

    #[test]
    fn listing_many_lines_partial_range() {
        assert_eq!(
            listing(
                "foo\nbar\nbaz\nqux\nfoo\nbar\nbaz\nqux\nfoo\nbar\nbaz\nqux",
                SourceRange { start: 33, end: 42 },
            ),
            " 9 \u{2502} foo\n   \u{250a}  \u{203e}\u{203e}\n10 \u{2502} bar\n   \u{250a} \
                \u{203e}\u{203e}\u{203e}\n11 \u{2502} baz\n     \u{203e}\u{203e}",
        );
    }

    #[test]
    fn format_errors_empty() {
        assert_eq!(format_errors(&[]), "");
    }

    #[test]
    fn format_errors_single() {
        assert_eq!(
            format_errors(&[Error::new("Something went wrong.")]),
            "[Error] Something went wrong.",
        );
    }

    #[test]
    fn format_errors_double() {
        assert_eq!(
            format_errors(&[
                Error::new("Something went kinda wrong."),
                Error::new("Something went sorta wrong."),
                Error::new("Something went very wrong."),
            ]),
            "\
[Error] Something went kinda wrong.

[Error] Something went sorta wrong.

[Error] Something went very wrong.\
",
        );
    }

    #[test]
    fn format_errors_visually_empty_line() {
        assert_eq!(
            format_errors(&[
                Error::new(
                    "1 \u{2502} foo\n  \u{250a} \u{203e}\u{203e}\u{203e}\n2 \u{2502} \
                                bar\n  \u{250a} \u{203e}\u{203e}\u{203e}\n3 \u{2502} baz\n  \
                                \u{250a} \u{203e}\u{203e}\u{203e}\n4 \u{2502} qux\n    \u{203e}\
                                \u{203e}\u{203e}",
                ),
                Error::new("Something went sorta wrong."),
            ]),
            "\
[Error] 1 \u{2502} foo\n  \u{250a} \u{203e}\u{203e}\u{203e}\n2 \u{2502} \
bar\n  \u{250a} \u{203e}\u{203e}\u{203e}\n3 \u{2502} baz\n  \
\u{250a} \u{203e}\u{203e}\u{203e}\n4 \u{2502} qux\n    \u{203e}\
\u{203e}\u{203e}
[Error] Something went sorta wrong.\
",
        );
    }
}
