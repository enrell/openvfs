//! A tiny shell-like tokenizer for sandbox commands.
//!
//! * Statements are separated by `;` or newlines.
//! * Words are separated by whitespace.
//! * `'single quotes'` are literal; `"double quotes"` understand `\n`, `\t`,
//!   `\"` and `\\` (any other backslash is kept, so Windows paths survive).
//! * `#` at the start of a word comments out the rest of the line.

/// Splits `src` into statements, each a list of words.
pub fn parse(src: &str) -> Result<Vec<Vec<String>>, String> {
    let mut p = Parser::default();
    let mut chars = src.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ';' | '\n' => p.end_statement(),
            c if c.is_whitespace() => p.end_word(),
            '#' if !p.in_word => while chars.next_if(|&n| n != '\n').is_some() {},
            '\'' => {
                p.in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => p.word.push(ch),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                p.in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => p.word.push('\n'),
                            Some('t') => p.word.push('\t'),
                            Some(ch @ ('"' | '\\')) => p.word.push(ch),
                            Some(other) => {
                                p.word.push('\\');
                                p.word.push(other);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(ch) => p.word.push(ch),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            c => {
                p.in_word = true;
                p.word.push(c);
            }
        }
    }
    p.end_statement();
    Ok(p.statements)
}

#[derive(Default)]
struct Parser {
    statements: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    /// Distinguishes an empty quoted argument (`""`) from no argument.
    in_word: bool,
}

impl Parser {
    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_statement(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.statements.push(std::mem::take(&mut self.words));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn words(src: &str) -> Vec<Vec<String>> {
        parse(src).unwrap()
    }

    #[test]
    fn splits_statements_and_words() {
        assert_eq!(words("ls src; cat  a.txt\nstatus"), [vec!["ls", "src"], vec!["cat", "a.txt"], vec!["status"]]);
    }

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(words(r#"write f "a b\nc" 'x;y' """#), [vec!["write", "f", "a b\nc", "x;y", ""]]);
        assert_eq!(words(r#"cat "C:\dir\x""#), [vec!["cat", r"C:\dir\x"]]);
    }

    #[test]
    fn comments() {
        assert_eq!(words("# header\nls # trailing\nab#c"), [vec!["ls"], vec!["ab#c"]]);
    }

    #[test]
    fn unterminated_quotes_fail() {
        assert!(parse("write 'oops").is_err());
        assert!(parse("write \"oops").is_err());
    }
}
