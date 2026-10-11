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

// Determine whether a backslash escapes a character in a context [tag:content_escapes]. Keep this
// in sync with `invalid_escape_message`.
pub fn is_escapable(character: char, context: Context) -> bool {
    match context {
        Context::Text => matches!(character, '\\' | '[' | ']' | '`'),
        Context::Link => matches!(character, '\\' | '[' | ']'),
        Context::Verbatim => false,
    }
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

// Determine whether a backslash starts an escape in a context, rather than being a plain
// character.
fn has_escapes(context: Context) -> bool {
    context != Context::Verbatim
}

#[cfg(test)]
mod tests {
    use super::{Context, escape, unescape};

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
}
