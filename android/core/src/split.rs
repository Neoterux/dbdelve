//! Where each statement in a buffer starts and ends, for the gutter mark and
//! for Run, which sends the statement under the cursor.
//!
//! Offsets are UTF-16 code units, how Kotlin indexes a `String`. A boundary
//! found wrong here sends more or less text than meant, never text the gate
//! has not read: what is sent is classified whole, by its own parse.

use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::tokenizer::{Location, Token, Tokenizer};

use crate::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

pub fn statements(engine: Engine, text: &str) -> Vec<Span> {
    let dialect: Box<dyn Dialect> = match engine {
        Engine::Postgres => Box::new(PostgreSqlDialect {}),
        Engine::MySql => Box::new(MySqlDialect {}),
        // JSON5's strings and brackets tokenize as SQL's do, which is all a
        // boundary needs.
        Engine::Mongo => Box::new(GenericDialect {}),
    };
    let Ok(tokens) = Tokenizer::new(dialect.as_ref(), text).tokenize_with_location() else {
        // An unterminated string or comment: the whole buffer is one statement
        // rather than a guess at where it might end.
        return whole(text);
    };
    let offsets = Offsets::new(text);
    let mut spans = Vec::new();
    let mut current: Option<Span> = None;
    for token in tokens {
        match token.token {
            Token::Whitespace(_) | Token::EOF => {}
            Token::SemiColon => spans.extend(current.take()),
            _ => {
                let start = offsets.at(token.span.start);
                let end = offsets.at(token.span.end);
                current = Some(match current {
                    Some(span) => Span {
                        start: span.start,
                        end,
                    },
                    None => Span { start, end },
                });
            }
        }
    }
    spans.extend(current);
    spans
}

/// The statement holding the cursor, or the last one before it when the cursor
/// sits after a `;` or in the blank lines between.
pub fn statement_at(engine: Engine, text: &str, cursor: u32) -> Option<Span> {
    let spans = statements(engine, text);
    spans
        .iter()
        .rev()
        .find(|span| span.start <= cursor)
        .or_else(|| spans.first())
        .copied()
}

fn whole(text: &str) -> Vec<Span> {
    let trimmed = text.trim_start();
    let start = utf16_len(&text[..text.len() - trimmed.len()]);
    let end = utf16_len(text.trim_end());
    if end > start {
        vec![Span { start, end }]
    } else {
        Vec::new()
    }
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Line and column, as the tokenizer counts them (1-based, in chars), to
/// UTF-16 offsets.
struct Offsets<'a> {
    lines: Vec<(&'a str, u32)>,
}

impl<'a> Offsets<'a> {
    fn new(text: &'a str) -> Self {
        let mut at = 0;
        let lines = text
            .split_inclusive('\n')
            .map(|line| {
                let start = at;
                at += utf16_len(line);
                (line, start)
            })
            .collect();
        Self { lines }
    }

    fn at(&self, location: Location) -> u32 {
        let Some(&(line, start)) = self.lines.get(location.line.saturating_sub(1) as usize) else {
            return self
                .lines
                .last()
                .map_or(0, |(line, start)| start + utf16_len(line));
        };
        let column = location.column.saturating_sub(1) as usize;
        start
            + line
                .chars()
                .take(column)
                .map(char::len_utf16)
                .sum::<usize>() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(engine: Engine, text: &str) -> Vec<String> {
        let units: Vec<u16> = text.encode_utf16().collect();
        statements(engine, text)
            .iter()
            .map(|span| String::from_utf16(&units[span.start as usize..span.end as usize]).unwrap())
            .collect()
    }

    #[test]
    fn splits_outside_strings_and_comments() {
        let sql = "-- head\nSELECT ';' AS a;\n\n/* ; */ UPDATE t SET b = 'é😀'\nWHERE id = 1;";
        assert_eq!(
            texts(Engine::Postgres, sql),
            ["SELECT ';' AS a", "UPDATE t SET b = 'é😀'\nWHERE id = 1"]
        );
        assert_eq!(
            texts(Engine::Postgres, "SELECT $$a;b$$"),
            ["SELECT $$a;b$$"]
        );
        assert_eq!(
            texts(Engine::Mongo, "db.t.find({a: ';'});\ndb.t.drop()"),
            ["db.t.find({a: ';'})", "db.t.drop()"]
        );
    }

    #[test]
    fn cursor_picks_its_statement() {
        let sql = "SELECT 1;\nSELECT 2;\n\n";
        let at = |cursor| {
            let span = statement_at(Engine::MySql, sql, cursor).unwrap();
            &sql[span.start as usize..span.end as usize]
        };
        assert_eq!(at(0), "SELECT 1");
        assert_eq!(at(12), "SELECT 2");
        assert_eq!(at(21), "SELECT 2");
    }

    #[test]
    fn unterminated_is_whole() {
        assert_eq!(
            texts(Engine::Postgres, "  SELECT 'open; "),
            ["SELECT 'open;"]
        );
    }
}
