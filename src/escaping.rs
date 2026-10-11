use std::iter;

// These functions convert between plain text, such as a page title or a path, and text as it
// appears in a page's content, where `[` and `]` delimit links and a backslash escapes the
// character after it [ref:content_escapes]. A heading isn't content: a page's title appears in its
// heading as is.

// Escape plain text, such as a page title or a path, so it retains its meaning as a link's target.
// Backticks are plain characters there, so they're left as written.
pub fn escape(plain: &str) -> String {
    plain
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

// Decode text written as content into plain text, as the parser does. There's no plain text if a
// backslash doesn't escape anything.
pub fn unescape(source: &str) -> Option<String> {
    let mut plain = String::new();
    let mut characters = source.chars();
    while let Some(character) = characters.next() {
        plain.push(if character == '\\' {
            characters.next().filter(|&next| is_escapable(next))?
        } else {
            character
        });
    }
    Some(plain)
}

// Scan text written as content, such as part of a wiki's source, for the characters which aren't
// escaped, such as unescaped link delimiters.
pub fn unescaped_characters(source: &str) -> impl Iterator<Item = (usize, char)> {
    let mut previous_was_escape = false;
    source.char_indices().filter(move |&(_, character)| {
        let is_escaped = previous_was_escape && is_escapable(character);
        previous_was_escape = character == '\\' && !is_escaped;
        !is_escaped
    })
}

// Find the backslashes in text written as content which don't escape anything, because they aren't
// followed by a character which can be escaped. Every other backslash escapes the next character,
// so a backslash itself is written as `\\`.
pub fn invalid_escapes(source: &str) -> impl Iterator<Item = usize> {
    let mut characters = source.char_indices().peekable();
    iter::from_fn(move || {
        while let Some((index, character)) = characters.next() {
            if character == '\\'
                && characters
                    .next_if(|&(_, next_character)| is_escapable(next_character))
                    .is_none()
            {
                return Some(index);
            }
        }
        None
    })
}

// Determine whether a backslash escapes a character in content [tag:content_escapes].
pub fn is_escapable(character: char) -> bool {
    matches!(character, '\\' | '[' | ']' | '`')
}

#[cfg(test)]
mod tests {
    use super::{escape, invalid_escapes, unescape, unescaped_characters};

    // Escape delimiters and backslashes in plain text, and decode escapes, but decode nothing when
    // a backslash doesn't escape anything.
    #[test]
    fn escaping() {
        assert_eq!(escape(r"a\[b]\c`"), r"a\\\[b\]\\c`");
        assert_eq!(
            unescape(r"a\\\[b\]\\c\\\``"),
            Some(r"a\[b]\c\``".to_owned()),
        );
        assert_eq!(unescape(r"\a"), None);
        assert_eq!(unescape(r"\#"), None);
        assert_eq!(unescape(r"a\\\"), None);
        assert_eq!(escape(r"#\#"), r"#\\#");
    }

    // Skip escaped characters, including an escaped backslash, which doesn't escape what follows.
    #[test]
    fn unescaped_characters_skip_escapes() {
        assert_eq!(
            unescaped_characters(r"[a\]\\]")
                .map(|(_, character)| character)
                .collect::<String>(),
            r"[a\\]",
        );
    }

    // Find each backslash which doesn't escape anything, including one at the end of the text.
    #[test]
    fn invalid_escapes_find_lone_backslashes() {
        assert_eq!(invalid_escapes(r"\a\\\[\").collect::<Vec<_>>(), vec![0, 6]);
    }
}
