//! Just enough SQL to read a `CREATE TABLE` statement as SQLite stores it
//! in the schema: the table's columns, their declared types, defaults and
//! constraints, and its options.

use crate::record::Value;
use crate::schema::{Affinity, Column, Generated};

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A bare identifier or keyword.
    Word(String),
    /// An identifier in `"…"`, `[…]` or `` `…` ``, unquoted.
    Quoted(String),
    /// A string literal, `'…'`, unquoted.
    Str(String),
    /// A blob literal, `x'…'`: its hex digits.
    Blob(String),
    /// A numeric literal as written.
    Number(String),
    /// Any other character.
    Punct(char),
}

impl Token {
    /// Whether this is the bare word `keyword` (case-insensitive).
    fn is(&self, keyword: &str) -> bool {
        matches!(self, Self::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    /// The name this token spells, if it can be an identifier.
    fn identifier(&self) -> Option<&str> {
        match self {
            Self::Word(name) | Self::Quoted(name) | Self::Str(name) => Some(name),
            _ => None,
        }
    }
}

/// Splits SQL text into tokens, dropping whitespace and comments.
struct Lexer {
    chars: Vec<char>,
    at: usize,
}

impl Lexer {
    fn tokens(sql: &str) -> Vec<Token> {
        let mut lexer = Self {
            chars: sql.chars().collect(),
            at: 0,
        };
        let mut tokens = Vec::new();
        while let Some(token) = lexer.next_token() {
            tokens.push(token);
        }
        tokens
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_blanks();
        let c = self.peek(0)?;
        let next = self.peek(1);
        Some(match c {
            '"' | '`' => Token::Quoted(self.delimited(c, c)),
            '[' => Token::Quoted(self.delimited('[', ']')),
            '\'' => Token::Str(self.delimited('\'', '\'')),
            'x' | 'X' if next == Some('\'') => {
                self.at += 1;
                Token::Blob(self.delimited('\'', '\''))
            }
            _ if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                Token::Number(self.number())
            }
            _ if is_identifier_char(c) => Token::Word(self.take_while(is_identifier_char)),
            _ => {
                self.at += 1;
                Token::Punct(c)
            }
        })
    }

    fn skip_blanks(&mut self) {
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some(c), _) if c.is_whitespace() => self.at += 1,
                (Some('-'), Some('-')) => {
                    self.take_while(|c| c != '\n');
                }
                (Some('/'), Some('*')) => {
                    self.at += 2;
                    while self.peek(0).is_some()
                        && (self.peek(0), self.peek(1)) != (Some('*'), Some('/'))
                    {
                        self.at += 1;
                    }
                    self.at = (self.at + 2).min(self.chars.len());
                }
                _ => return,
            }
        }
    }

    /// Text between `open` and `close`, where a doubled `close` stands for
    /// itself (except in `[…]`).
    fn delimited(&mut self, open: char, close: char) -> String {
        self.at += 1;
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            self.at += 1;
            if c != close {
                text.push(c);
            } else if open != '[' && self.peek(0) == Some(close) {
                self.at += 1;
                text.push(c);
            } else {
                break;
            }
        }
        text
    }

    fn number(&mut self) -> String {
        if self.peek(0) == Some('0') && matches!(self.peek(1), Some('x' | 'X')) {
            self.at += 2;
            return format!("0x{}", self.take_while(|c| c.is_ascii_hexdigit()));
        }
        let mut text = self.take_while(|c| c.is_ascii_digit() || c == '.');
        if matches!(self.peek(0), Some('e' | 'E')) {
            text.push('e');
            self.at += 1;
            if let Some(sign @ ('+' | '-')) = self.peek(0) {
                text.push(sign);
                self.at += 1;
            }
            text += &self.take_while(|c| c.is_ascii_digit());
        }
        text
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let start = self.at;
        while self.peek(0).is_some_and(&keep) {
            self.at += 1;
        }
        self.chars[start..self.at].iter().collect()
    }
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// What a `CREATE TABLE` statement declares.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Definition {
    /// An ordinary table.
    Table {
        columns: Vec<Column>,
        without_rowid: bool,
    },
    /// `CREATE VIRTUAL TABLE`: rows come from a module, not a b-tree.
    Virtual,
}

/// Words that end a column's type name and start a constraint.
const CONSTRAINT_WORDS: [&str; 11] = [
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
];
/// Words that start a table constraint rather than a column.
const TABLE_CONSTRAINT_WORDS: [&str; 5] = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// Read a `CREATE TABLE` statement, or `None` when it isn't one this
/// parser understands.
pub(crate) fn parse_create_table(sql: &str) -> Option<Definition> {
    let tokens = Lexer::tokens(sql);
    let mut stream = Stream {
        tokens: &tokens,
        at: 0,
    };
    if !stream.accept("CREATE") {
        return None;
    }
    stream.accept_any(&["TEMP", "TEMPORARY"]);
    if stream.accept("VIRTUAL") {
        return Some(Definition::Virtual);
    }
    let if_not_exists = |stream: &mut Stream| {
        !stream.accept("IF") || (stream.accept("NOT") && stream.accept("EXISTS"))
    };
    if !(stream.accept("TABLE") && if_not_exists(&mut stream) && stream.accept_name()) {
        return None;
    }
    let (body, options) = parenthesized(stream.rest())?;
    let without_rowid = options
        .windows(2)
        .any(|pair| pair[0].is("WITHOUT") && pair[1].is("ROWID"));
    let columns = columns(body, without_rowid)?;
    Some(Definition::Table {
        columns,
        without_rowid,
    })
}

/// Tokens read from the front.
struct Stream<'t> {
    tokens: &'t [Token],
    at: usize,
}

impl<'t> Stream<'t> {
    /// Step past the bare word `keyword`, if it is next.
    fn accept(&mut self, keyword: &str) -> bool {
        let found = self
            .tokens
            .get(self.at)
            .is_some_and(|token| token.is(keyword));
        self.at += usize::from(found);
        found
    }

    /// Step past whichever of `keywords` is next, if one is.
    fn accept_any(&mut self, keywords: &[&str]) -> bool {
        keywords.iter().any(|keyword| self.accept(keyword))
    }

    /// Step past an object name, perhaps qualified by a schema name.
    fn accept_name(&mut self) -> bool {
        let is_name = |at: usize| self.tokens.get(at).and_then(Token::identifier).is_some();
        let qualified = self.tokens.get(self.at + 1) == Some(&Token::Punct('.'));
        let length = if qualified { 3 } else { 1 };
        let found = is_name(self.at) && is_name(self.at + length - 1);
        self.at += if found { length } else { 0 };
        found
    }

    fn rest(&self) -> &'t [Token] {
        &self.tokens[self.at..]
    }
}

/// The tokens inside the parentheses that `tokens` starts with, and those
/// after the closing one.
fn parenthesized(tokens: &[Token]) -> Option<(&[Token], &[Token])> {
    if tokens.first() != Some(&Token::Punct('(')) {
        return None;
    }
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some((&tokens[1..index], &tokens[index + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// `tokens` split at the commas outside parentheses.
fn split_top_level(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[start..]);
    parts
}

/// The columns declared in a table's body, with the rowid alias marked.
fn columns(body: &[Token], without_rowid: bool) -> Option<Vec<Column>> {
    let mut columns = Vec::new();
    let mut declarations = Vec::new();
    let mut table_primary_key = Vec::new();
    for part in split_top_level(body) {
        let first = part.first()?;
        if TABLE_CONSTRAINT_WORDS.iter().any(|word| first.is(word)) {
            table_primary_key.extend(primary_key_columns(part));
        } else {
            let (column, declaration) = column(part)?;
            columns.push(column);
            declarations.push(declaration);
        }
    }
    if let [name] = table_primary_key.as_slice() {
        // In a table constraint even `PRIMARY KEY(x DESC)` makes an alias.
        if let Some(index) = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
        {
            declarations[index] = PrimaryKey::Ascending;
        }
    }
    if !without_rowid {
        if let Some(index) = rowid_alias(&columns, &declarations) {
            columns[index].rowid_alias = true;
        }
    }
    Some(columns)
}

/// How a column takes part in the primary key, as far as aliasing the
/// rowid goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimaryKey {
    No,
    Ascending,
    /// `PRIMARY KEY DESC` on the column itself, which (a documented quirk)
    /// does not alias the rowid.
    Descending,
}

/// The one column that aliases the rowid: the sole primary key column,
/// declared exactly `INTEGER`, not `PRIMARY KEY DESC`.
fn rowid_alias(columns: &[Column], declarations: &[PrimaryKey]) -> Option<usize> {
    let keys: Vec<usize> = (0..columns.len())
        .filter(|&index| declarations[index] != PrimaryKey::No)
        .collect();
    let [index] = keys.as_slice() else {
        return None;
    };
    let integer = columns[*index]
        .declared_type
        .eq_ignore_ascii_case("INTEGER");
    (integer && declarations[*index] == PrimaryKey::Ascending).then_some(*index)
}

/// The column names of a `PRIMARY KEY (…)` table constraint, if `part` is
/// one.
fn primary_key_columns(part: &[Token]) -> Vec<String> {
    let Some(start) = part.iter().position(|token| token.is("PRIMARY")) else {
        return Vec::new();
    };
    let Some((list, _)) = part.get(start + 2..).and_then(parenthesized) else {
        return Vec::new();
    };
    split_top_level(list)
        .iter()
        .filter_map(|item| item.first()?.identifier().map(str::to_owned))
        .collect()
}

/// A column definition: its name, type name, then constraints.
fn column(part: &[Token]) -> Option<(Column, PrimaryKey)> {
    let name = part.first()?.identifier()?.to_owned();
    let (declared_type, constraints) = type_name(&part[1..]);
    let mut column = Column {
        name,
        affinity: Affinity::of_declared_type(&declared_type),
        declared_type,
        rowid_alias: false,
        default: Value::Null,
        generated: None,
    };
    let primary_key = column_constraints(constraints, &mut column);
    Some((column, primary_key))
}

/// The type name at the start of `tokens` (words, then perhaps a
/// parenthesized size), normalized to single spaces, and the tokens after.
fn type_name(tokens: &[Token]) -> (String, &[Token]) {
    let words = tokens
        .iter()
        .take_while(|token| {
            matches!(token, Token::Word(_) | Token::Quoted(_))
                && !CONSTRAINT_WORDS.iter().any(|word| token.is(word))
        })
        .count();
    let mut name: Vec<String> = tokens[..words]
        .iter()
        .filter_map(|token| token.identifier().map(str::to_owned))
        .collect();
    let mut rest = &tokens[words..];
    if words > 0 {
        if let Some((size, after)) = parenthesized(rest) {
            let size: Vec<String> = size.iter().map(token_text).collect();
            if let Some(last) = name.last_mut() {
                *last = format!("{last}({})", size.join(""));
            }
            rest = after;
        }
    }
    (name.join(" "), rest)
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Word(text)
        | Token::Quoted(text)
        | Token::Str(text)
        | Token::Blob(text)
        | Token::Number(text) => text.clone(),
        Token::Punct(c) => c.to_string(),
    }
}

/// Read the constraints that matter for reading rows: `PRIMARY KEY`,
/// `DEFAULT` and `AS` (generated columns). Parenthesized parts (checks,
/// expressions) are skipped.
fn column_constraints(tokens: &[Token], column: &mut Column) -> PrimaryKey {
    let mut primary_key = PrimaryKey::No;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if token.is("PRIMARY") => {
                let descending = tokens.get(index + 2).is_some_and(|t| t.is("DESC"));
                primary_key = if descending {
                    PrimaryKey::Descending
                } else {
                    PrimaryKey::Ascending
                };
            }
            _ if token.is("DEFAULT") => column.default = literal(&tokens[index + 1..]),
            _ if token.is("AS") => column.generated = Some(generated(&tokens[index + 1..])),
            _ => {}
        }
    }
    primary_key
}

/// `STORED` or `VIRTUAL` after a generated column's expression; virtual
/// when neither is said.
fn generated(tokens: &[Token]) -> Generated {
    let after = parenthesized(tokens).map_or(tokens, |(_, after)| after);
    if after.first().is_some_and(|token| token.is("STORED")) {
        Generated::Stored
    } else {
        Generated::Virtual
    }
}

/// A default value written as a literal; `NULL` for anything else (an
/// expression, `CURRENT_TIME`), which this reader doesn't evaluate.
fn literal(tokens: &[Token]) -> Value {
    match tokens {
        [Token::Punct('-'), Token::Number(number), ..] => number_value(number, true),
        [Token::Punct('+'), Token::Number(number), ..] | [Token::Number(number), ..] => {
            number_value(number, false)
        }
        [Token::Str(text), ..] => Value::Text(text.clone()),
        [Token::Blob(hex), ..] => hex_bytes(hex).map_or(Value::Null, Value::Blob),
        [word, ..] if word.is("TRUE") => Value::Integer(1),
        [word, ..] if word.is("FALSE") => Value::Integer(0),
        _ => Value::Null,
    }
}

/// A numeric literal: hexadecimal and decimal integers that fit 64 bits are
/// integers, everything else a real.
fn number_value(text: &str, negative: bool) -> Value {
    if let Some(hex) = text.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).map_or(Value::Null, |bits| {
            let integer = bits as i64;
            Value::Integer(if negative {
                integer.wrapping_neg()
            } else {
                integer
            })
        });
    }
    let signed = if negative {
        format!("-{text}")
    } else {
        text.to_owned()
    };
    match signed.parse::<i64>() {
        Ok(integer) => Value::Integer(integer),
        Err(_) => signed.parse::<f64>().map_or(Value::Null, Value::Real),
    }
}

fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(sql: &str) -> (Vec<Column>, bool) {
        match parse_create_table(sql) {
            Some(Definition::Table {
                columns,
                without_rowid,
            }) => (columns, without_rowid),
            other => panic!("{sql}: {other:?}"),
        }
    }

    fn names(sql: &str) -> Vec<String> {
        table(sql).0.into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn quoted_names_and_comments() {
        let sql =
            "CREATE TABLE \"t\" (\"odd \"\"name\"\"\" TEXT, [bracket col] BLOB, `tick` REAL, \
                   'str' -- a comment\n, /* inline */ plain)";
        assert_eq!(
            names(sql),
            ["odd \"name\"", "bracket col", "tick", "str", "plain"]
        );
    }

    #[test]
    fn table_constraints_are_not_columns() {
        let sql = "CREATE TABLE t(a, b, CONSTRAINT pk PRIMARY KEY (a, b), UNIQUE(b), \
                   CHECK (a > 0), FOREIGN KEY (b) REFERENCES u(x))";
        assert_eq!(names(sql), ["a", "b"]);
    }

    #[test]
    fn types_keep_their_size() {
        let (columns, _) =
            table("CREATE TABLE t(a VARCHAR(10) NOT NULL, b DECIMAL (10, 5), c UNSIGNED BIG INT)");
        let types: Vec<_> = columns.iter().map(|c| c.declared_type.as_str()).collect();
        assert_eq!(types, ["VARCHAR(10)", "DECIMAL(10,5)", "UNSIGNED BIG INT"]);
        assert_eq!(columns[2].affinity, Affinity::Integer);
    }

    #[test]
    fn integer_primary_key_aliases_the_rowid() {
        let alias = |sql| table(sql).0.iter().position(|c| c.rowid_alias);
        assert_eq!(alias("CREATE TABLE t(a, id INTEGER PRIMARY KEY)"), Some(1));
        assert_eq!(
            alias("CREATE TABLE t(x integer, y, PRIMARY KEY(x DESC))"),
            Some(0)
        );
        assert_eq!(alias("CREATE TABLE t(x INTEGER PRIMARY KEY DESC, y)"), None);
        assert_eq!(alias("CREATE TABLE t(x INT PRIMARY KEY)"), None);
        assert_eq!(
            alias("CREATE TABLE t(x INTEGER, y, PRIMARY KEY(x, y))"),
            None
        );
        assert_eq!(
            alias("CREATE TABLE t(x INTEGER PRIMARY KEY, y) WITHOUT ROWID"),
            None
        );
    }

    #[test]
    fn defaults_and_generated_columns() {
        let (columns, _) = table(
            "CREATE TABLE t(a DEFAULT -5, b DEFAULT 'x''y', c DEFAULT x'00ff', d DEFAULT (1 + 2), \
             e DEFAULT 1.5e3, f AS (a * 2), g TEXT GENERATED ALWAYS AS (upper(b)) STORED)",
        );
        let defaults: Vec<_> = columns.iter().map(|c| c.default.clone()).collect();
        assert_eq!(
            defaults[..5],
            [
                Value::Integer(-5),
                Value::Text("x'y".to_owned()),
                Value::Blob(vec![0, 0xff]),
                Value::Null,
                Value::Real(1500.0),
            ]
        );
        assert_eq!(columns[5].generated, Some(Generated::Virtual));
        assert_eq!(columns[6].generated, Some(Generated::Stored));
    }

    #[test]
    fn options_and_virtual_tables() {
        assert!(table("CREATE TABLE t(a PRIMARY KEY) WITHOUT ROWID, STRICT").1);
        assert_eq!(
            parse_create_table("CREATE VIRTUAL TABLE f USING fts5(body)"),
            Some(Definition::Virtual)
        );
        assert_eq!(parse_create_table("CREATE VIEW v AS SELECT 1"), None);
    }
}
