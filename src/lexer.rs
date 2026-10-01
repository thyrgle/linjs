//! The linjs lexer: source text to tokens with absolute byte spans.
//!
//! Comments (`//`, `/* */`) are skipped but their spans are recorded —
//! the M3 memory annotations (`// @own`, `// @ref`) will be recovered
//! from these.

/// A token kind. Keyword and punctuation kinds are exact; literals carry
/// their parsed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals
    Num(f64),
    Str(String),
    Ident(String),
    // keywords
    Let,
    Const,
    Var,
    As,
    Function,
    Return,
    If,
    Else,
    While,
    For,
    Of,
    In,
    Break,
    Continue,
    True,
    False,
    Null,
    Undefined,
    Typeof,
    // punctuation and operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Question,
    Colon,
    Arrow,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    Inc,
    Dec,
    Not,
    Eq,
    Ne,
    EqStrict,
    NeStrict,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    UShr,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    // end of input
    Eof,
}

/// One token: kind plus its absolute byte span in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub start: usize,
    pub end: usize,
}

/// A lexing failure: the byte offset and a message.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub at: usize,
    pub message: String,
}

/// A skipped comment, kept for the memory annotations (`// @own`,
/// `// @ref`) that live inside them.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub start: usize,
    pub end: usize,
    /// The text between the comment opener and end of comment, trimmed.
    pub text: String,
}

impl Comment {
    /// The annotation mode the comment declares, if any: the trimmed
    /// text starts with `@own` or `@ref` (trailing prose allowed).
    pub fn annotation(&self) -> Option<crate::ast::Mem> {
        let text = self.text.trim();
        if text.starts_with("@own") {
            Some(crate::ast::Mem::Own)
        } else if text.starts_with("@ref") {
            Some(crate::ast::Mem::Ref)
        } else {
            None
        }
    }
}

/// The output of lexing: tokens plus the comments that were skipped.
#[derive(Debug, Clone)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
}

pub fn lex(source: &str) -> Result<Lexed, LexError> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut comments = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let start = i;
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                comments.push(Comment {
                    start,
                    end: i,
                    text: source[start + 2..i].to_string(),
                });
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return Err(LexError {
                        at: start,
                        message: "unterminated block comment".into(),
                    });
                }
                i += 2;
                comments.push(Comment {
                    start,
                    end: i,
                    text: source[start + 2..i - 2].to_string(),
                });
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let text = &source[start..i];
                let value: f64 = text.parse().map_err(|_| LexError {
                    at: start,
                    message: format!("bad number `{text}`"),
                })?;
                tokens.push(Token {
                    kind: Tok::Num(value),
                    start,
                    end: i,
                });
            }
            b'"' | b'\'' => {
                let start = i;
                let quote = b;
                i += 1;
                let mut out = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            at: start,
                            message: "unterminated string".into(),
                        });
                    }
                    match bytes[i] {
                        b'\\' => {
                            if i + 1 >= bytes.len() {
                                return Err(LexError {
                                    at: start,
                                    message: "unterminated escape".into(),
                                });
                            }
                            let esc = bytes[i + 1];
                            out.push(match esc {
                                b'n' => '\n',
                                b't' => '\t',
                                b'r' => '\r',
                                b'0' => '\0',
                                other => other as char,
                            });
                            i += 2;
                        }
                        b if b == quote => {
                            i += 1;
                            break;
                        }
                        _ => {
                            let ch = source[i..].chars().next().unwrap();
                            out.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                tokens.push(Token {
                    kind: Tok::Str(out),
                    start,
                    end: i,
                });
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                let word = &source[start..i];
                let kind = match word {
                    "let" => Tok::Let,
                    "const" => Tok::Const,
                    "var" => Tok::Var,
                    "as" => Tok::As,
                    "function" => Tok::Function,
                    "return" => Tok::Return,
                    "if" => Tok::If,
                    "else" => Tok::Else,
                    "while" => Tok::While,
                    "for" => Tok::For,
                    "of" => Tok::Of,
                    "in" => Tok::In,
                    "break" => Tok::Break,
                    "continue" => Tok::Continue,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "null" => Tok::Null,
                    "undefined" => Tok::Undefined,
                    "typeof" => Tok::Typeof,
                    _ => Tok::Ident(word.to_string()),
                };
                tokens.push(Token {
                    kind,
                    start,
                    end: i,
                });
            }
            _ => {
                let (kind, len) = punct(bytes, i)?;
                tokens.push(Token {
                    kind,
                    start: i,
                    end: i + len,
                });
                i += len;
            }
        }
    }
    tokens.push(Token {
        kind: Tok::Eof,
        start: bytes.len(),
        end: bytes.len(),
    });
    Ok(Lexed { tokens, comments })
}

/// Matches the longest punctuation/operator at `i`.
fn punct(bytes: &[u8], i: usize) -> Result<(Tok, usize), LexError> {
    let two = |s: &[u8]| bytes.len() >= i + 2 && &bytes[i..i + 2] == s;
    let three = |s: &[u8]| bytes.len() >= i + 3 && &bytes[i..i + 3] == s;
    let ok = |tok: Tok, len: usize| Ok((tok, len));
    match () {
        _ if three(b"===") => ok(Tok::EqStrict, 3),
        _ if three(b"!==") => ok(Tok::NeStrict, 3),
        _ if three(b">>>") => ok(Tok::UShr, 3),
        _ if two(b"=>") => ok(Tok::Arrow, 2),
        _ if two(b"==") => ok(Tok::Eq, 2),
        _ if two(b"!=") => ok(Tok::Ne, 2),
        _ if two(b"<=") => ok(Tok::Le, 2),
        _ if two(b">=") => ok(Tok::Ge, 2),
        _ if two(b"<<") => ok(Tok::Shl, 2),
        _ if two(b">>") => ok(Tok::Shr, 2),
        _ if two(b"&&") => ok(Tok::And, 2),
        _ if two(b"||") => ok(Tok::Or, 2),
        _ if two(b"++") => ok(Tok::Inc, 2),
        _ if two(b"--") => ok(Tok::Dec, 2),
        _ if two(b"+=") => ok(Tok::PlusAssign, 2),
        _ if two(b"-=") => ok(Tok::MinusAssign, 2),
        _ if two(b"*=") => ok(Tok::StarAssign, 2),
        _ if two(b"/=") => ok(Tok::SlashAssign, 2),
        _ if bytes[i] == b'(' => ok(Tok::LParen, 1),
        _ if bytes[i] == b')' => ok(Tok::RParen, 1),
        _ if bytes[i] == b'{' => ok(Tok::LBrace, 1),
        _ if bytes[i] == b'}' => ok(Tok::RBrace, 1),
        _ if bytes[i] == b'[' => ok(Tok::LBracket, 1),
        _ if bytes[i] == b']' => ok(Tok::RBracket, 1),
        _ if bytes[i] == b';' => ok(Tok::Semi, 1),
        _ if bytes[i] == b',' => ok(Tok::Comma, 1),
        _ if bytes[i] == b'.' => ok(Tok::Dot, 1),
        _ if bytes[i] == b'?' => ok(Tok::Question, 1),
        _ if bytes[i] == b':' => ok(Tok::Colon, 1),
        _ if bytes[i] == b'!' => ok(Tok::Not, 1),
        _ if bytes[i] == b'&' => ok(Tok::Amp, 1),
        _ if bytes[i] == b'|' => ok(Tok::Pipe, 1),
        _ if bytes[i] == b'^' => ok(Tok::Caret, 1),
        _ if bytes[i] == b'~' => ok(Tok::Tilde, 1),
        _ if bytes[i] == b'<' => ok(Tok::Lt, 1),
        _ if bytes[i] == b'>' => ok(Tok::Gt, 1),
        _ if bytes[i] == b'=' => ok(Tok::Assign, 1),
        _ if bytes[i] == b'+' => ok(Tok::Plus, 1),
        _ if bytes[i] == b'-' => ok(Tok::Minus, 1),
        _ if bytes[i] == b'*' => ok(Tok::Star, 1),
        _ if bytes[i] == b'/' => ok(Tok::Slash, 1),
        _ if bytes[i] == b'%' => ok(Tok::Percent, 1),
        _ => Err(LexError {
            at: i,
            message: format!("unexpected character `{}`", bytes[i] as char),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_basic_tokens() {
        let toks = lex("let x = 1.5; // c\nx === 'a\\n'").unwrap().tokens;
        let kinds: Vec<&Tok> = toks.iter().map(|t| &t.kind).collect();
        assert_eq!(
            kinds,
            [
                &Tok::Let,
                &Tok::Ident("x".into()),
                &Tok::Assign,
                &Tok::Num(1.5),
                &Tok::Semi,
                &Tok::Ident("x".into()),
                &Tok::EqStrict,
                &Tok::Str("a\n".into()),
                &Tok::Eof,
            ]
        );
    }

    #[test]
    fn spans_are_absolute() {
        let toks = lex("ab + 1").unwrap().tokens;
        assert_eq!((toks[0].start, toks[0].end), (0, 2));
        assert_eq!((toks[2].start, toks[2].end), (5, 6));
    }

    #[test]
    fn rejects_bad_characters() {
        assert!(lex("let @x;").is_err());
        assert!(lex("/* nope").is_err());
    }

    #[test]
    fn records_comments_with_annotations() {
        let lexed = lex("// @own\nlet a = [1]; // just a note").unwrap();
        assert_eq!(lexed.comments.len(), 2);
        assert_eq!(lexed.comments[0].annotation(), Some(crate::ast::Mem::Own));
        assert_eq!(lexed.comments[1].annotation(), None);
        // Line comments stop before the newline.
        assert_eq!(lexed.comments[0].text, " @own");
    }
}
