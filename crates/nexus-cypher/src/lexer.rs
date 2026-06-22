//! Tokenizer for openCypher queries.

use crate::error::{CypherError, CypherResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Match,
    Where,
    Return,
    As,
    Order,
    By,
    Limit,
    Skip,
    Distinct,
    And,
    Or,
    Not,
    Null,
    True,
    False,
    Is,
    Contains,
    StartsWith,
    EndsWith,
    Asc,
    Desc,
    Create,
    Delete,
    Detach,
    Set,
    Remove,
    Merge,
    On,
    With,
    Unwind,
    Optional,
    In,
    Case,
    When,
    Then,
    Else,
    End,
    Union,
    All,
    Count,
    Exists,

    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    Pipe,
    Star,
    DashArrowRight,
    ArrowLeftDash,
    Dash,
    Plus,
    Slash,
    Percent,
    Caret,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    DotDot,
    PlusEq,

    Ident(String),
    IntLiteral(i64),
    IntLiteralMinMagnitude,
    FloatLiteral(f64),
    StringLiteral(String),
    Parameter(String),
    Eof,
}

pub struct Lexer {
    input: Vec<char>,
    pos: usize,
}

impl Lexer {
    pub fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            pos: 0,
        }
    }

    pub fn tokenize(&mut self) -> CypherResult<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            if self.pos >= self.input.len() {
                tokens.push(Token::Eof);
                break;
            }
            tokens.push(self.next_token()?);
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }
    fn advance(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        self.pos += 1;
        c
    }

    fn peek_non_whitespace(&self) -> Option<char> {
        let mut pos = self.pos;
        while pos < self.input.len() && self.input[pos].is_whitespace() {
            pos += 1;
        }
        self.input.get(pos).copied()
    }

    fn skip_whitespace(&mut self) {
        loop {
            // Skip whitespace.
            while self.pos < self.input.len() && self.input[self.pos].is_whitespace() {
                self.pos += 1;
            }
            // Line comment: `// ...` through end of line.
            if self.pos + 1 < self.input.len()
                && self.input[self.pos] == '/'
                && self.input[self.pos + 1] == '/'
            {
                while self.pos < self.input.len() && self.input[self.pos] != '\n' {
                    self.pos += 1;
                }
                continue;
            }
            // Block comment: `/* ... */`.
            if self.pos + 1 < self.input.len()
                && self.input[self.pos] == '/'
                && self.input[self.pos + 1] == '*'
            {
                self.pos += 2;
                while self.pos + 1 < self.input.len()
                    && !(self.input[self.pos] == '*' && self.input[self.pos + 1] == '/')
                {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.input.len());
                continue;
            }
            break;
        }
    }

    fn next_token(&mut self) -> CypherResult<Token> {
        let c = self.peek().unwrap();
        match c {
            '(' => {
                self.advance();
                Ok(Token::LParen)
            }
            ')' => {
                self.advance();
                Ok(Token::RParen)
            }
            '[' => {
                self.advance();
                Ok(Token::LBracket)
            }
            ']' => {
                self.advance();
                Ok(Token::RBracket)
            }
            '{' => {
                self.advance();
                Ok(Token::LBrace)
            }
            '}' => {
                self.advance();
                Ok(Token::RBrace)
            }
            ':' => {
                self.advance();
                Ok(Token::Colon)
            }
            ',' => {
                self.advance();
                Ok(Token::Comma)
            }
            '.' => {
                if self
                    .input
                    .get(self.pos + 1)
                    .is_some_and(|c| c.is_ascii_digit())
                {
                    self.read_fractional_float()
                } else {
                    self.advance();
                    if self.peek() == Some('.') {
                        self.advance();
                        Ok(Token::DotDot)
                    } else {
                        Ok(Token::Dot)
                    }
                }
            }
            '|' => {
                self.advance();
                Ok(Token::Pipe)
            }
            '*' => {
                self.advance();
                Ok(Token::Star)
            }
            '+' => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(Token::PlusEq)
                } else {
                    Ok(Token::Plus)
                }
            }
            '/' => {
                self.advance();
                Ok(Token::Slash)
            }
            '%' => {
                self.advance();
                Ok(Token::Percent)
            }
            '^' => {
                self.advance();
                Ok(Token::Caret)
            }
            '=' => {
                self.advance();
                Ok(Token::Eq)
            }
            '`' => self.read_backticked_ident(),
            '<' => {
                self.advance();
                match self.peek() {
                    Some('=') => {
                        self.advance();
                        Ok(Token::Lte)
                    }
                    Some('>') => {
                        self.advance();
                        Ok(Token::Neq)
                    }
                    Some('-') => {
                        self.advance();
                        Ok(Token::ArrowLeftDash)
                    }
                    _ => Ok(Token::Lt),
                }
            }
            '>' => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(Token::Gte)
                } else {
                    Ok(Token::Gt)
                }
            }
            '-' => {
                self.advance();
                if self.peek() == Some('>') {
                    self.advance();
                    Ok(Token::DashArrowRight)
                } else {
                    Ok(Token::Dash)
                }
            }
            '$' => {
                self.advance();
                let name = self.read_ident();
                Ok(Token::Parameter(name))
            }
            '\'' | '"' => self.read_string(),
            _ if c.is_ascii_digit() => self.read_number(),
            _ if c.is_alphabetic() || c == '_' => {
                let ident = self.read_ident();
                if self.peek_non_whitespace() == Some(':') {
                    return Ok(Token::Ident(ident));
                }
                Ok(Self::keyword_or_ident(ident))
            }
            _ => Err(CypherError::Parse {
                position: self.pos,
                message: format!("unexpected character: '{c}'"),
            }),
        }
    }

    fn read_backticked_ident(&mut self) -> CypherResult<Token> {
        self.advance(); // consume opening backtick
        let mut s = String::new();
        while self.pos < self.input.len() {
            let c = self.advance().unwrap();
            if c == '`' {
                return Ok(Token::Ident(s));
            }
            s.push(c);
        }
        Err(CypherError::Parse {
            position: self.pos,
            message: "unterminated backticked identifier".into(),
        })
    }

    fn read_ident(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.input.len()
            && (self.input[self.pos].is_alphanumeric() || self.input[self.pos] == '_')
        {
            self.pos += 1;
        }
        self.input[start..self.pos].iter().collect()
    }

    fn read_string(&mut self) -> CypherResult<Token> {
        let quote = self.advance().unwrap();
        let mut s = String::new();
        while self.pos < self.input.len() {
            let c = self.advance().unwrap();
            if c == quote {
                return Ok(Token::StringLiteral(s));
            }
            if c == '\\' && self.pos < self.input.len() {
                let escaped = self.advance().unwrap();
                match escaped {
                    'b' => s.push('\u{0008}'),
                    'f' => s.push('\u{000c}'),
                    'n' => s.push('\n'),
                    'r' => s.push('\r'),
                    't' => s.push('\t'),
                    '\\' => s.push('\\'),
                    '\'' => s.push('\''),
                    '"' => s.push('"'),
                    'u' => s.push(self.read_unicode_escape()?),
                    c if c == quote => s.push(c),
                    _ => {
                        s.push('\\');
                        s.push(escaped);
                    }
                }
            } else {
                s.push(c);
            }
        }
        Err(CypherError::Parse {
            position: self.pos,
            message: "unterminated string".into(),
        })
    }

    fn read_unicode_escape(&mut self) -> CypherResult<char> {
        let start = self.pos.saturating_sub(2);
        if self.pos + 4 > self.input.len() {
            return Err(CypherError::Parse {
                position: start,
                message: "invalid unicode escape".into(),
            });
        }
        let digits: String = self.input[self.pos..self.pos + 4].iter().collect();
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CypherError::Parse {
                position: start,
                message: format!("invalid unicode escape: \\u{digits}"),
            });
        }
        self.pos += 4;
        let code = u32::from_str_radix(&digits, 16).map_err(|_| CypherError::Parse {
            position: start,
            message: format!("invalid unicode escape: \\u{digits}"),
        })?;
        char::from_u32(code).ok_or_else(|| CypherError::Parse {
            position: start,
            message: format!("invalid unicode escape: \\u{digits}"),
        })
    }

    fn read_number(&mut self) -> CypherResult<Token> {
        let start = self.pos;
        if self.input.get(self.pos) == Some(&'0')
            && self
                .input
                .get(self.pos + 1)
                .is_some_and(|c| *c == 'x' || *c == 'X')
        {
            self.pos += 2;
            let digits_start = self.pos;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            if self.pos == digits_start {
                return Err(CypherError::Parse {
                    position: start,
                    message: "invalid hexadecimal integer".into(),
                });
            }
            self.validate_numeric_boundary(start)?;
            let digits: String = self.input[digits_start..self.pos].iter().collect();
            return self.integer_token_from_magnitude(start, &digits, 16);
        }

        while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.pos < self.input.len()
            && self.input[self.pos] == '.'
            && self.input.get(self.pos + 1) != Some(&'.')
            && self
                .input
                .get(self.pos + 1)
                .is_some_and(|c| c.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < self.input.len()
            && matches!(self.input[self.pos], 'e' | 'E')
            && self.input.get(self.pos + 1).is_some_and(|c| {
                c.is_ascii_digit()
                    || ((*c == '+' || *c == '-')
                        && self
                            .input
                            .get(self.pos + 2)
                            .is_some_and(|next| next.is_ascii_digit()))
            })
        {
            is_float = true;
            self.pos += 1;
            if self.pos < self.input.len() && matches!(self.input[self.pos], '+' | '-') {
                self.pos += 1;
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }

        let s: String = self.input[start..self.pos].iter().collect();
        self.validate_numeric_boundary(start)?;
        if is_float {
            self.float_token(start, &s)
        } else if s.len() > 1 && s.starts_with('0') {
            self.integer_token_from_magnitude(start, &s, 8)
        } else {
            self.integer_token_from_magnitude(start, &s, 10)
        }
    }

    fn read_fractional_float(&mut self) -> CypherResult<Token> {
        let start = self.pos;
        self.pos += 1; // leading dot
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos < self.input.len()
            && matches!(self.input[self.pos], 'e' | 'E')
            && self.input.get(self.pos + 1).is_some_and(|c| {
                c.is_ascii_digit()
                    || ((*c == '+' || *c == '-')
                        && self
                            .input
                            .get(self.pos + 2)
                            .is_some_and(|next| next.is_ascii_digit()))
            })
        {
            self.pos += 1;
            if self.pos < self.input.len() && matches!(self.input[self.pos], '+' | '-') {
                self.pos += 1;
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        let s: String = self.input[start..self.pos].iter().collect();
        self.validate_numeric_boundary(start)?;
        self.float_token(start, &format!("0{s}"))
    }

    fn validate_numeric_boundary(&self, start: usize) -> CypherResult<()> {
        if self
            .input
            .get(self.pos)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
        {
            let mut end = self.pos;
            while end < self.input.len()
                && (self.input[end].is_alphanumeric() || self.input[end] == '_')
            {
                end += 1;
            }
            let literal: String = self.input[start..end].iter().collect();
            return Err(CypherError::Parse {
                position: start,
                message: format!("invalid numeric literal: {literal}"),
            });
        }
        Ok(())
    }

    fn float_token(&self, start: usize, s: &str) -> CypherResult<Token> {
        let v: f64 = s.parse().map_err(|_| CypherError::Parse {
            position: start,
            message: format!("invalid float: {s}"),
        })?;
        if !v.is_finite() {
            return Err(CypherError::Parse {
                position: start,
                message: format!("float out of range: {s}"),
            });
        }
        Ok(Token::FloatLiteral(v))
    }

    fn integer_token_from_magnitude(
        &self,
        start: usize,
        s: &str,
        radix: u32,
    ) -> CypherResult<Token> {
        let v = u128::from_str_radix(s, radix).map_err(|_| CypherError::Parse {
            position: start,
            message: format!("invalid integer: {s}"),
        })?;
        if v <= i64::MAX as u128 {
            Ok(Token::IntLiteral(v as i64))
        } else if v == (i64::MAX as u128) + 1 {
            Ok(Token::IntLiteralMinMagnitude)
        } else {
            Err(CypherError::Parse {
                position: start,
                message: format!("integer out of range: {s}"),
            })
        }
    }

    fn keyword_or_ident(s: String) -> Token {
        match s.to_uppercase().as_str() {
            "MATCH" => Token::Match,
            "WHERE" => Token::Where,
            "RETURN" => Token::Return,
            "AS" => Token::As,
            "ORDER" => Token::Order,
            "BY" => Token::By,
            "LIMIT" => Token::Limit,
            "SKIP" => Token::Skip,
            "DISTINCT" => Token::Distinct,
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            "NULL" => Token::Null,
            "TRUE" => Token::True,
            "FALSE" => Token::False,
            "IS" => Token::Is,
            "CONTAINS" => Token::Contains,
            "STARTS" => Token::StartsWith,
            "ENDS" => Token::EndsWith,
            "ASC" | "ASCENDING" => Token::Asc,
            "DESC" | "DESCENDING" => Token::Desc,
            "CREATE" => Token::Create,
            "DELETE" => Token::Delete,
            "DETACH" => Token::Detach,
            "SET" => Token::Set,
            "REMOVE" => Token::Remove,
            "MERGE" => Token::Merge,
            "ON" => Token::On,
            "WITH" => Token::With,
            "UNWIND" => Token::Unwind,
            "OPTIONAL" => Token::Optional,
            "IN" => Token::In,
            "CASE" => Token::Case,
            "WHEN" => Token::When,
            "THEN" => Token::Then,
            "ELSE" => Token::Else,
            "END" => Token::End,
            "UNION" => Token::Union,
            "ALL" => Token::All,
            "COUNT" => Token::Count,
            "EXISTS" => Token::Exists,
            _ => Token::Ident(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_simple_match() {
        let mut lexer = Lexer::new("MATCH (n:Entity) RETURN n");
        let tokens = lexer.tokenize().unwrap();
        assert_eq!(tokens[0], Token::Match);
        assert_eq!(tokens[1], Token::LParen);
        assert_eq!(tokens[2], Token::Ident("n".into()));
        assert_eq!(tokens[3], Token::Colon);
        assert_eq!(tokens[4], Token::Ident("Entity".into()));
        assert_eq!(tokens[5], Token::RParen);
        assert_eq!(tokens[6], Token::Return);
    }

    #[test]
    fn lex_relationship() {
        let mut lexer = Lexer::new("-[:DISCLOSES]->");
        let tokens = lexer.tokenize().unwrap();
        assert_eq!(tokens[0], Token::Dash);
        assert_eq!(tokens[1], Token::LBracket);
        assert_eq!(tokens[2], Token::Colon);
        assert_eq!(tokens[3], Token::Ident("DISCLOSES".into()));
        assert_eq!(tokens[4], Token::RBracket);
        assert_eq!(tokens[5], Token::DashArrowRight);
    }

    #[test]
    fn lex_where_clause() {
        let mut lexer = Lexer::new("WHERE n.name = 'Apple' AND n.revenue > 100");
        let tokens = lexer.tokenize().unwrap();
        assert_eq!(tokens[0], Token::Where);
        assert_eq!(tokens[1], Token::Ident("n".into()));
        assert_eq!(tokens[2], Token::Dot);
        assert_eq!(tokens[3], Token::Ident("name".into()));
        assert_eq!(tokens[4], Token::Eq);
        assert_eq!(tokens[5], Token::StringLiteral("Apple".into()));
        assert_eq!(tokens[6], Token::And);
    }

    #[test]
    fn lex_open_cypher_numeric_literal_forms() {
        let mut lexer = Lexer::new("0x1A 026 .5 1e3 9223372036854775808");
        let tokens = lexer.tokenize().unwrap();
        assert_eq!(tokens[0], Token::IntLiteral(26));
        assert_eq!(tokens[1], Token::IntLiteral(22));
        assert_eq!(tokens[2], Token::FloatLiteral(0.5));
        assert_eq!(tokens[3], Token::FloatLiteral(1000.0));
        assert_eq!(tokens[4], Token::IntLiteralMinMagnitude);
    }

    #[test]
    fn lex_rejects_infinite_float_literal() {
        let mut lexer = Lexer::new("1.34E999");
        assert!(lexer.tokenize().is_err());
    }

    #[test]
    fn lex_rejects_alphanumeric_numeric_suffixes() {
        for query in ["9223372h54775808", "0x1A2b3j4D5E6f7", "0x1A2b3c4Z5E6f7"] {
            let mut lexer = Lexer::new(query);
            assert!(lexer.tokenize().is_err(), "{query} should be invalid");
        }
    }

    #[test]
    fn lex_open_cypher_string_escapes() {
        let mut lexer = Lexer::new(r#"'\'' '\u01FF' "a\n\t\"b""#);
        let tokens = lexer.tokenize().unwrap();
        assert_eq!(tokens[0], Token::StringLiteral("'".into()));
        assert_eq!(tokens[1], Token::StringLiteral("ǿ".into()));
        assert_eq!(tokens[2], Token::StringLiteral("a\n\t\"b".into()));
    }

    #[test]
    fn lex_rejects_invalid_unicode_escape() {
        let mut lexer = Lexer::new(r#"'\uH'"#);
        assert!(lexer.tokenize().is_err());
    }
}
