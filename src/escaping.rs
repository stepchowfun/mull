use std::iter;

// These functions convert between plain text, such as a page title or a path, and text as it
// appears in a wiki's source, where `[` and `]` delimit links, backticks delimit code, and a
// backslash escapes the character after it [ref:content_escapes]. Which characters a backslash can
// escape depends on the context, since backticks are plain characters in a link's target, and
// title lines and code are taken as written.

// This is where text appears in a wiki's source, which determines how it's escaped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Text,     // Content outside links and code
    Link,     // Between a link's delimiters
    Verbatim, // A title line or code, where a backslash is a plain character
}

// Escape plain text, such as a page title or a path, so it retains its meaning in a context.
pub fn escape(plain: &str, context: Context) -> String {
    let mut escaped = String::new();
    for character in plain.chars() {
        if is_escapable(character, context) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

// Decode text written in a context into plain text, as the parser does. There's no plain text if a
// backslash doesn't escape anything.
pub fn unescape(source: &str, context: Context) -> Option<String> {
    let mut plain = String::new();
    let mut characters = source.chars();
    while let Some(character) = characters.next() {
        plain.push(if character == '\\' && has_escapes(context) {
            characters
                .next()
                .filter(|&next| is_escapable(next, context))?
        } else {
            character
        });
    }
    Some(plain)
}

// Scan text written as content, such as part of a wiki's source, for the characters which aren't
// escaped, such as unescaped link delimiters. This recognizes the escapes of text, which include
// those of a link, and finds the same delimiters as recognizing only a link's escapes would, since
// a backtick isn't a link delimiter.
pub fn unescaped_characters(source: &str) -> impl Iterator<Item = (usize, char)> {
    let mut previous_was_escape = false;
    source.char_indices().filter(move |&(_, character)| {
        let is_escaped = previous_was_escape && is_escapable(character, Context::Text);
        previous_was_escape = character == '\\' && !is_escaped;
        !is_escaped
    })
}

// Find the backslashes in text written in a context which don't escape anything, because they
// aren't followed by a character which can be escaped there. Every other backslash escapes the
// next character, so a backslash itself is written as `\\`.
pub fn invalid_escapes(source: &str, context: Context) -> impl Iterator<Item = usize> {
    let mut characters = source.char_indices().peekable();
    iter::from_fn(move || {
        while let Some((index, character)) = characters.next() {
            if character == '\\'
                && has_escapes(context)
                && characters
                    .next_if(|&(_, next_character)| is_escapable(next_character, context))
                    .is_none()
            {
                return Some(index);
            }
        }
        None
    })
}

// Determine whether a backslash escapes a character in a context [tag:content_escapes]. Keep this
// in sync with `invalid_escape_message`.
pub fn is_escapable(character: char, context: Context) -> bool {
    match context {
        Context::Text => matches!(character, '\\' | '[' | ']' | '`'),
        Context::Link => matches!(character, '\\' | '[' | ']'),
        Context::Verbatim => false,
    }
}

// Determine whether a backslash starts an escape in a context, rather than being a plain
// character.
fn has_escapes(context: Context) -> bool {
    context != Context::Verbatim
}

// Explain which characters a backslash can escape in a context.
pub fn invalid_escape_message(context: Context) -> &'static str {
    match context {
        Context::Text => {
            "A backslash must be followed by `[`, `]`, `` ` ``, or `\\`, so write `\\\\` for a \
             backslash itself."
        }
        Context::Link => {
            "In a link, a backslash must be followed by `[`, `]`, or `\\`, so write `\\\\` for a \
             backslash itself."
        }
        Context::Verbatim => "Here, a backslash is a plain character.",
    }
}

#[cfg(test)]
mod tests {
    use super::{Context, escape, invalid_escapes, unescape, unescaped_characters};

    // Escape delimiters and backslashes in plain text, and decode escapes, but decode nothing when
    // a backslash doesn't escape anything. Backticks are escaped only outside links, and verbatim
    // text is left as written.
    #[test]
    fn escaping() {
        assert_eq!(escape(r"a\[b]\c`", Context::Text), r"a\\\[b\]\\c\`");
        assert_eq!(escape(r"a\[b]\c`", Context::Link), r"a\\\[b\]\\c`");
        assert_eq!(
            unescape(r"a\\\[b\]\\c\\\``", Context::Text),
            Some(r"a\[b]\c\``".to_owned()),
        );
        assert_eq!(
            unescape(r"a\\\[b\]\\c\\`", Context::Link),
            Some(r"a\[b]\c\`".to_owned()),
        );
        assert_eq!(unescape(r"\`", Context::Link), None);
        assert_eq!(unescape(r"\a", Context::Text), None);
        assert_eq!(unescape(r"\#", Context::Text), None);
        assert_eq!(unescape(r"a\\\", Context::Text), None);
        assert_eq!(escape(r"#\#", Context::Text), r"#\\#");
        assert_eq!(escape(r"a\[b]\c`", Context::Verbatim), r"a\[b]\c`");
        assert_eq!(
            unescape(r"a\[b]\c`\", Context::Verbatim),
            Some(r"a\[b]\c`\".to_owned()),
        );
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

    // Find each backslash which doesn't escape anything in its context, including one at the end
    // of the text, but none in verbatim text, where backslashes are plain characters.
    #[test]
    fn invalid_escapes_find_lone_backslashes() {
        assert_eq!(
            invalid_escapes(r"\a\\\[\`\", Context::Text).collect::<Vec<_>>(),
            vec![0, 8],
        );
        assert_eq!(
            invalid_escapes(r"\a\\\[\`\", Context::Link).collect::<Vec<_>>(),
            vec![0, 6, 8],
        );
        assert_eq!(
            invalid_escapes(r"\a\\\[\`\", Context::Verbatim).collect::<Vec<_>>(),
            Vec::<usize>::new(),
        );
    }
}
