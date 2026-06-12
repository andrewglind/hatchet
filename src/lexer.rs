//! Haxe lexer.
//!
//! Turns source text into a flat `Vec<Token>` (plus a trailing `Eof`). Comments
//! and whitespace are discarded; line numbers are tracked for diagnostics.
//!
//! Notable Haxe specifics handled here:
//!   * single-quoted strings interpolate (`'${x}'`), double-quoted do not — the
//!     `interpolated` flag records which, the inner text is kept raw;
//!   * `1...6` lexes as `Int(1) "..." Int(6)`, while `1.0` is a `Float`;
//!   * metadata `@:name` / `@name` become `Meta` tokens; any `(...)` arguments are
//!     left as ordinary tokens for the parser to consume.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokKind {
    Ident(String),
    Kw(Kw),
    Int(String),
    Float(String),
    Str { raw: String, interpolated: bool },
    /// `@:name` or `@name` — the leading `@`/`@:` is stripped, name retained.
    Meta(String),
    /// A regular-expression literal `~/pattern/flags` — the raw pattern and flag
    /// letters, slashes stripped. Hatchet does not transpile regex (it is flagged
    /// `Unsupported`), but it is lexed so the diagnostic is clean.
    Regex { pattern: String, flags: String },
    /// A Haxe conditional-compilation directive. Hatchet only targets C++, so
    /// these are not used for *Haxe* conditional compilation; instead they are
    /// repurposed as a clean front end for the C++ preprocessor (`#if FLAG` →
    /// `#ifdef FLAG`, `#else`, `#end` → `#endif`). `If`/`ElseIf` carry the raw
    /// condition text; `Else`/`End` carry an empty string.
    Pp(PpKind, String),
    Sym(Sym),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpKind {
    If,
    ElseIf,
    Else,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Package, Import, Using,
    Class, Interface, Enum, Typedef, Abstract,
    Extends, Implements,
    Function, Var, Final,
    Public, Private, Static, Inline, Extern, Override, Dynamic, Macro,
    New, Return, If, Else, For, While, Do,
    Switch, Case, Default, Break, Continue,
    True, False, Null, This, Super, Cast, In,
    Throw, Try, Catch, Untyped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sym {
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Semi, Comma, Colon, Dot, DotDotDot,
    Arrow,      // ->
    FatArrow,   // =>
    Question, QuestionDot, QuestionQuestion, QuestionQuestionEq,
    Assign, Eq, Ne, Lt, Gt, Le, Ge,
    Plus, Minus, Star, Slash, Percent,
    PlusEq, MinusEq, StarEq, SlashEq, PercentEq,
    PlusPlus, MinusMinus,
    AmpAmp, PipePipe, Bang,
    Amp, Pipe, Caret, Tilde, Shl, Shr, UShr,
    AmpEq, PipeEq, CaretEq, ShlEq, ShrEq, UShrEq,
    At,
    Dollar,     // $ — only appears in Haxe macro reification, which is unsupported
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub line: usize,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error (line {}): {}", self.line, self.message)
    }
}

impl std::error::Error for LexError {}

pub fn keyword(word: &str) -> Option<Kw> {
    use Kw::*;
    Some(match word {
        "package" => Package, "import" => Import, "using" => Using,
        "class" => Class, "interface" => Interface, "enum" => Enum,
        "typedef" => Typedef, "abstract" => Abstract,
        "extends" => Extends, "implements" => Implements,
        "function" => Function, "var" => Var, "final" => Final,
        "public" => Public, "private" => Private, "static" => Static,
        "inline" => Inline, "extern" => Extern, "override" => Override,
        "dynamic" => Dynamic, "macro" => Macro,
        "new" => New, "return" => Return, "if" => If, "else" => Else,
        "for" => For, "while" => While, "do" => Do,
        "switch" => Switch, "case" => Case, "default" => Default,
        "break" => Break, "continue" => Continue,
        "true" => True, "false" => False, "null" => Null,
        "this" => This, "super" => Super, "cast" => Cast, "in" => In,
        "throw" => Throw, "try" => Try, "catch" => Catch, "untyped" => Untyped,
        _ => return None,
    })
}

struct Lexer<'a> {
    bytes: &'a [u8],
    i: usize,
    line: usize,
    out: Vec<Token>,
}

/// Tokenize Haxe source.
pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer {
        bytes: src.as_bytes(),
        i: 0,
        line: 1,
        out: Vec::new(),
    };
    lx.run()?;
    Ok(lx.out)
}

impl<'a> Lexer<'a> {
    fn run(&mut self) -> Result<(), LexError> {
        loop {
            self.skip_trivia()?;
            if self.i >= self.bytes.len() {
                break;
            }
            let start = self.i;
            let c = self.bytes[self.i];
            let kind = match c {
                b'#' => self.lex_directive()?,
                b'~' if self.i + 1 < self.bytes.len() && self.bytes[self.i + 1] == b'/' => {
                    self.lex_regex()?
                }
                b'"' | b'\'' => self.lex_string(c)?,
                b'@' => self.lex_meta()?,
                _ if c.is_ascii_digit() => self.lex_number()?,
                _ if is_ident_start(c) => self.lex_ident(),
                _ => self.lex_symbol()?,
            };
            let end = self.i;
            let line = self.line;
            self.out.push(Token { kind, line, start, end });
        }
        self.out.push(Token {
            kind: TokKind::Eof,
            line: self.line,
            start: self.i,
            end: self.i,
        });
        Ok(())
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        let n = self.bytes.len();
        while self.i < n {
            let c = self.bytes[self.i];
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'/' if self.i + 1 < n && self.bytes[self.i + 1] == b'/' => {
                    self.i += 2;
                    while self.i < n && self.bytes[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                b'/' if self.i + 1 < n && self.bytes[self.i + 1] == b'*' => {
                    self.i += 2;
                    loop {
                        if self.i + 1 >= n {
                            return Err(self.err("unterminated block comment"));
                        }
                        if self.bytes[self.i] == b'*' && self.bytes[self.i + 1] == b'/' {
                            self.i += 2;
                            break;
                        }
                        if self.bytes[self.i] == b'\n' {
                            self.line += 1;
                        }
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn lex_string(&mut self, quote: u8) -> Result<TokKind, LexError> {
        let n = self.bytes.len();
        self.i += 1; // opening quote
        let content_start = self.i;
        while self.i < n {
            let c = self.bytes[self.i];
            if c == b'\\' && self.i + 1 < n {
                if self.bytes[self.i + 1] == b'\n' {
                    self.line += 1;
                }
                self.i += 2;
                continue;
            }
            if c == b'\n' {
                self.line += 1;
            }
            if c == quote {
                let raw = String::from_utf8_lossy(&self.bytes[content_start..self.i]).into_owned();
                self.i += 1; // closing quote
                return Ok(TokKind::Str {
                    raw,
                    interpolated: quote == b'\'',
                });
            }
            self.i += 1;
        }
        Err(self.err("unterminated string literal"))
    }

    /// Lex a conditional-compilation directive (`#if`/`#elseif`/`#else`/`#end`).
    /// For `#if`/`#elseif` the remainder of the line (sans any `//` comment) is
    /// captured raw as the condition; the parser validates and translates it.
    fn lex_directive(&mut self) -> Result<TokKind, LexError> {
        self.i += 1; // '#'
        let name_start = self.i;
        while self.i < self.bytes.len() && is_ident_continue(self.bytes[self.i]) {
            self.i += 1;
        }
        let name = self.slice(name_start);
        let kind = match name.as_str() {
            "if" => PpKind::If,
            "elseif" => PpKind::ElseIf,
            "else" => return Ok(TokKind::Pp(PpKind::Else, String::new())),
            "end" => return Ok(TokKind::Pp(PpKind::End, String::new())),
            "" => return Err(self.err("expected a directive name after '#'")),
            other => {
                return Err(self.err(&format!(
                    "unsupported conditional-compilation directive '#{other}'"
                )))
            }
        };
        let cond_start = self.i;
        while self.i < self.bytes.len() && self.bytes[self.i] != b'\n' {
            self.i += 1;
        }
        let raw = self.slice(cond_start);
        let cond = raw.split("//").next().unwrap_or("").trim().to_string();
        Ok(TokKind::Pp(kind, cond))
    }

    /// Lex a regular-expression literal `~/pattern/flags`. The pattern runs to the
    /// next unescaped `/` (a `\/` is an escaped slash, kept); flags are the trailing
    /// ASCII letters. Slashes are stripped from both captured parts.
    fn lex_regex(&mut self) -> Result<TokKind, LexError> {
        let n = self.bytes.len();
        self.i += 2; // skip `~/`
        let pat_start = self.i;
        loop {
            if self.i >= n {
                return Err(self.err("unterminated regular-expression literal"));
            }
            match self.bytes[self.i] {
                b'\\' if self.i + 1 < n => self.i += 2, // escaped char (e.g. `\/`)
                b'\n' => return Err(self.err("unterminated regular-expression literal")),
                b'/' => break,
                _ => self.i += 1,
            }
        }
        let pattern = self.slice(pat_start);
        self.i += 1; // closing `/`
        let flag_start = self.i;
        while self.i < n && self.bytes[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        let flags = self.slice(flag_start);
        Ok(TokKind::Regex { pattern, flags })
    }

    fn lex_meta(&mut self) -> Result<TokKind, LexError> {
        // '@' already at self.i
        self.i += 1;
        if self.i < self.bytes.len() && self.bytes[self.i] == b':' {
            self.i += 1;
        }
        let start = self.i;
        while self.i < self.bytes.len() && is_ident_continue(self.bytes[self.i]) {
            self.i += 1;
        }
        if start == self.i {
            // a bare '@' with no name — emit the At symbol instead
            return Ok(TokKind::Sym(Sym::At));
        }
        Ok(TokKind::Meta(
            String::from_utf8_lossy(&self.bytes[start..self.i]).into_owned(),
        ))
    }

    fn lex_number(&mut self) -> Result<TokKind, LexError> {
        let n = self.bytes.len();
        let start = self.i;
        // hex
        if self.bytes[self.i] == b'0'
            && self.i + 1 < n
            && (self.bytes[self.i + 1] | 0x20) == b'x'
        {
            self.i += 2;
            while self.i < n && self.bytes[self.i].is_ascii_hexdigit() {
                self.i += 1;
            }
            return Ok(TokKind::Int(self.slice(start)));
        }
        while self.i < n && self.bytes[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let mut is_float = false;
        // fractional part — but only if not the `...` range operator
        if self.i < n && self.bytes[self.i] == b'.' && !(self.i + 1 < n && self.bytes[self.i + 1] == b'.') {
            is_float = true;
            self.i += 1;
            while self.i < n && self.bytes[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        // exponent
        if self.i < n && (self.bytes[self.i] | 0x20) == b'e' {
            let save = self.i;
            self.i += 1;
            if self.i < n && (self.bytes[self.i] == b'+' || self.bytes[self.i] == b'-') {
                self.i += 1;
            }
            if self.i < n && self.bytes[self.i].is_ascii_digit() {
                is_float = true;
                while self.i < n && self.bytes[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            } else {
                self.i = save; // not an exponent
            }
        }
        if is_float {
            Ok(TokKind::Float(self.slice(start)))
        } else {
            Ok(TokKind::Int(self.slice(start)))
        }
    }

    fn lex_ident(&mut self) -> TokKind {
        let start = self.i;
        while self.i < self.bytes.len() && is_ident_continue(self.bytes[self.i]) {
            self.i += 1;
        }
        let word = self.slice(start);
        match keyword(&word) {
            Some(kw) => TokKind::Kw(kw),
            None => TokKind::Ident(word),
        }
    }

    fn lex_symbol(&mut self) -> Result<TokKind, LexError> {
        let n = self.bytes.len();
        let c = self.bytes[self.i];
        let c1 = if self.i + 1 < n { self.bytes[self.i + 1] } else { 0 };
        let c2 = if self.i + 2 < n { self.bytes[self.i + 2] } else { 0 };
        let c3 = if self.i + 3 < n { self.bytes[self.i + 3] } else { 0 };
        use Sym::*;

        // four-character (`>>>=` must win over `>>>`)
        if (c, c1, c2, c3) == (b'>', b'>', b'>', b'=') {
            self.i += 4;
            return Ok(TokKind::Sym(UShrEq));
        }

        // three-character
        let three = match (c, c1, c2) {
            (b'.', b'.', b'.') => Some(DotDotDot),
            (b'?', b'?', b'=') => Some(QuestionQuestionEq),
            (b'<', b'<', b'=') => Some(ShlEq),
            (b'>', b'>', b'=') => Some(ShrEq),
            (b'>', b'>', b'>') => Some(UShr),
            _ => None,
        };
        if let Some(s) = three {
            self.i += 3;
            return Ok(TokKind::Sym(s));
        }

        // two-character
        let two = match (c, c1) {
            (b'-', b'>') => Some(Arrow),
            (b'=', b'>') => Some(FatArrow),
            (b'?', b'?') => Some(QuestionQuestion),
            (b'?', b'.') => Some(QuestionDot),
            (b'=', b'=') => Some(Eq),
            (b'!', b'=') => Some(Ne),
            (b'<', b'=') => Some(Le),
            (b'>', b'=') => Some(Ge),
            (b'&', b'&') => Some(AmpAmp),
            (b'|', b'|') => Some(PipePipe),
            (b'+', b'+') => Some(PlusPlus),
            (b'-', b'-') => Some(MinusMinus),
            (b'+', b'=') => Some(PlusEq),
            (b'-', b'=') => Some(MinusEq),
            (b'*', b'=') => Some(StarEq),
            (b'/', b'=') => Some(SlashEq),
            (b'%', b'=') => Some(PercentEq),
            (b'&', b'=') => Some(AmpEq),
            (b'|', b'=') => Some(PipeEq),
            (b'^', b'=') => Some(CaretEq),
            (b'<', b'<') => Some(Shl),
            (b'>', b'>') => Some(Shr),
            _ => None,
        };
        if let Some(s) = two {
            self.i += 2;
            return Ok(TokKind::Sym(s));
        }

        // one-character
        let one = match c {
            b'(' => LParen, b')' => RParen,
            b'{' => LBrace, b'}' => RBrace,
            b'[' => LBracket, b']' => RBracket,
            b';' => Semi, b',' => Comma, b':' => Colon, b'.' => Dot,
            b'?' => Question,
            b'=' => Assign, b'<' => Lt, b'>' => Gt,
            b'+' => Plus, b'-' => Minus, b'*' => Star, b'/' => Slash, b'%' => Percent,
            b'!' => Bang, b'&' => Amp, b'|' => Pipe, b'^' => Caret, b'~' => Tilde,
            b'$' => Dollar,
            _ => return Err(self.err(&format!("unexpected character '{}'", c as char))),
        };
        self.i += 1;
        Ok(TokKind::Sym(one))
    }

    fn slice(&self, start: usize) -> String {
        String::from_utf8_lossy(&self.bytes[start..self.i]).into_owned()
    }

    fn err(&self, msg: &str) -> LexError {
        LexError {
            message: msg.to_string(),
            line: self.line,
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).filter(|k| *k != TokKind::Eof).collect()
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            kinds("class Foo"),
            vec![TokKind::Kw(Kw::Class), TokKind::Ident("Foo".into())]
        );
    }

    #[test]
    fn range_vs_float() {
        // 1...6 -> Int Sym(...) Int
        assert_eq!(
            kinds("1...6"),
            vec![
                TokKind::Int("1".into()),
                TokKind::Sym(Sym::DotDotDot),
                TokKind::Int("6".into())
            ]
        );
        // 10.0 -> Float
        assert_eq!(kinds("10.0"), vec![TokKind::Float("10.0".into())]);
    }

    #[test]
    fn hex_and_exponent() {
        assert_eq!(kinds("0xFFDDDDDD"), vec![TokKind::Int("0xFFDDDDDD".into())]);
        assert_eq!(kinds("1e3"), vec![TokKind::Float("1e3".into())]);
    }

    #[test]
    fn regex_literal_tokenization() {
        // `~/pattern/flags` → one Regex token; slashes stripped, flags captured.
        assert_eq!(
            kinds("~/haxe/i"),
            vec![TokKind::Regex { pattern: "haxe".into(), flags: "i".into() }]
        );
        // An escaped slash `\/` stays part of the pattern; no flags is fine.
        assert_eq!(
            kinds(r"~/a\/b/"),
            vec![TokKind::Regex { pattern: r"a\/b".into(), flags: "".into() }]
        );
        // A bare `~` is still bitwise-not when not followed by `/`.
        assert_eq!(kinds("~x"), vec![TokKind::Sym(Sym::Tilde), TokKind::Ident("x".into())]);
    }

    #[test]
    fn strings_interpolation_flag() {
        assert_eq!(
            kinds("'${x}_y'"),
            vec![TokKind::Str { raw: "${x}_y".into(), interpolated: true }]
        );
        assert_eq!(
            kinds("\"plain\""),
            vec![TokKind::Str { raw: "plain".into(), interpolated: false }]
        );
    }

    #[test]
    fn metadata() {
        assert_eq!(kinds("@:native"), vec![TokKind::Meta("native".into())]);
        assert_eq!(
            kinds("@:readOnly public"),
            vec![TokKind::Meta("readOnly".into()), TokKind::Kw(Kw::Public)]
        );
    }

    #[test]
    fn operators() {
        use Sym::*;
        assert_eq!(
            kinds("a ?? b => c -> d ?.e ??= f"),
            vec![
                TokKind::Ident("a".into()), TokKind::Sym(QuestionQuestion),
                TokKind::Ident("b".into()), TokKind::Sym(FatArrow),
                TokKind::Ident("c".into()), TokKind::Sym(Arrow),
                TokKind::Ident("d".into()), TokKind::Sym(QuestionDot),
                TokKind::Ident("e".into()), TokKind::Sym(QuestionQuestionEq),
                TokKind::Ident("f".into()),
            ]
        );
    }

    #[test]
    fn comments_skipped_and_lines_tracked() {
        let toks = lex("a // c\n/* x\ny */ b").unwrap();
        assert_eq!(toks[0].kind, TokKind::Ident("a".into()));
        assert_eq!(toks[1].kind, TokKind::Ident("b".into()));
        assert_eq!(toks[1].line, 3);
    }
}
