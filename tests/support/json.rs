//! Just enough JSON for `sqlite3 -json` output: an array of flat objects
//! whose values are strings, integers or null.

/// An object's members, in order.
pub type Row = Vec<(String, Value)>;

/// A member's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Text(String),
}

/// The objects of a JSON array (empty input: none, as `sqlite3 -json`
/// prints nothing for no rows).
pub fn rows(text: &str) -> Vec<Row> {
    let mut parser = Parser {
        chars: text.chars().collect(),
        at: 0,
    };
    if parser.skip_blanks().is_none() {
        return Vec::new();
    }
    parser.list('[', ']', Parser::object)
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    /// Skip whitespace; the next character, if any.
    fn skip_blanks(&mut self) -> Option<char> {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
        self.chars.get(self.at).copied()
    }

    fn expect(&mut self, expected: char) {
        assert_eq!(self.skip_blanks(), Some(expected), "at {}", self.at);
        self.at += 1;
    }

    /// Items between `open` and `close`, separated by commas.
    fn list<T>(&mut self, open: char, close: char, item: fn(&mut Self) -> T) -> Vec<T> {
        self.expect(open);
        let mut items = Vec::new();
        if self.skip_blanks() == Some(close) {
            self.at += 1;
            return items;
        }
        loop {
            items.push(item(self));
            match self.skip_blanks() {
                Some(',') => self.at += 1,
                _ => break,
            }
        }
        self.expect(close);
        items
    }

    fn object(&mut self) -> Row {
        self.list('{', '}', |parser| {
            let name = parser.string();
            parser.expect(':');
            (name, parser.value())
        })
    }

    fn value(&mut self) -> Value {
        match self.skip_blanks() {
            Some('"') => Value::Text(self.string()),
            Some('n') => {
                self.at += 4;
                Value::Null
            }
            _ => {
                let start = self.at;
                while self
                    .chars
                    .get(self.at)
                    .is_some_and(|c| *c == '-' || c.is_ascii_digit())
                {
                    self.at += 1;
                }
                let digits: String = self.chars[start..self.at].iter().collect();
                Value::Integer(digits.parse().unwrap())
            }
        }
    }

    fn string(&mut self) -> String {
        self.expect('"');
        let mut text = String::new();
        loop {
            match self.next_char() {
                '"' => return text,
                '\\' => text.push(self.escape()),
                c => text.push(c),
            }
        }
    }

    fn next_char(&mut self) -> char {
        let c = self.chars[self.at];
        self.at += 1;
        c
    }

    /// The character a backslash escape stands for.
    fn escape(&mut self) -> char {
        match self.next_char() {
            'u' => self.unicode_escape(),
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'b' => '\u{8}',
            'f' => '\u{c}',
            other => other,
        }
    }

    /// `\uXXXX`, followed by a second one when the first is a high
    /// surrogate.
    fn unicode_escape(&mut self) -> char {
        let first = self.hex_unit();
        let mut units = vec![first];
        if (0xd800..0xdc00).contains(&first) {
            self.at += 2; // the second escape's backslash and `u`
            units.push(self.hex_unit());
        }
        char::decode_utf16(units).next().unwrap().unwrap()
    }

    fn hex_unit(&mut self) -> u16 {
        let hex: String = self.chars[self.at..self.at + 4].iter().collect();
        self.at += 4;
        u16::from_str_radix(&hex, 16).unwrap()
    }
}
