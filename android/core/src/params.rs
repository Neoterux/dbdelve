//! Named parameters in a saved query: `:name`, filled in with values the user
//! types when running it.
//!
//! A value goes in as a literal of the engine's own spelling, never as text
//! spliced into the statement: a string is quoted, a number must read as one,
//! and NULL is NULL. The filled statement is shown before it runs and is
//! classified whole by the gate, so a value cannot carry anything past it.
//! Placeholders are found with the tokenizer, so `':name'` inside a string,
//! a comment or a Postgres `::cast` is never one.

use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::tokenizer::{Location, Token, Tokenizer};

use crate::{CoreError, Engine};

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ParamValue {
    Text { value: String },
    Number { value: String },
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Binding {
    pub name: String,
    pub value: ParamValue,
}

/// Where each `:name` is, as byte ranges over the text.
fn placeholders(engine: Engine, text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let dialect: Box<dyn Dialect> = match engine {
        Engine::Postgres => Box::new(PostgreSqlDialect {}),
        Engine::MySql => Box::new(MySqlDialect {}),
        Engine::Mongo => Box::new(GenericDialect {}),
    };
    let Ok(tokens) = Tokenizer::new(dialect.as_ref(), text).tokenize_with_location() else {
        return Vec::new();
    };
    let bytes = Bytes::new(text);
    let mut found = Vec::new();
    for pair in tokens.windows(2) {
        let (colon, word) = (&pair[0], &pair[1]);
        let Token::Colon = colon.token else { continue };
        let Token::Word(name) = &word.token else {
            continue;
        };
        let start = bytes.at(colon.span.start);
        // The name follows the colon directly, and the colon does not follow
        // a name, number or bracket directly: `{a:true}` is a key and a
        // value, `a[1:2]` a slice, `{a: :v}` a parameter.
        let glued = bytes.at(colon.span.end) == bytes.at(word.span.start);
        let after_value = text[..start].chars().next_back().is_some_and(|c| {
            c.is_alphanumeric() || matches!(c, '_' | ')' | ']' | '}' | '"' | '\'' | '`')
        });
        let plain = name.quote_style.is_none()
            && name
                .value
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name
                .value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if glued && !after_value && plain {
            found.push((start..bytes.at(word.span.end), name.value.clone()));
        }
    }
    found
}

/// The parameters a query asks for, each once, in the order they appear.
pub fn parameters(engine: Engine, text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, name) in placeholders(engine, text) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The query with every `:name` replaced by its value's literal.
pub fn fill(engine: Engine, text: &str, bindings: &[Binding]) -> Result<String, CoreError> {
    let refused = |reason: String| CoreError::Query { reason };
    let mut filled = String::with_capacity(text.len());
    let mut at = 0;
    for (range, name) in placeholders(engine, text) {
        let binding = bindings
            .iter()
            .find(|binding| binding.name == name)
            .ok_or_else(|| refused(format!("No value for :{name}.")))?;
        filled.push_str(&text[at..range.start]);
        filled.push_str(
            &literal(engine, &binding.value)
                .map_err(|reason| refused(format!(":{name}: {reason}")))?,
        );
        at = range.end;
    }
    filled.push_str(&text[at..]);
    Ok(filled)
}

fn literal(engine: Engine, value: &ParamValue) -> Result<String, String> {
    Ok(match value {
        ParamValue::Null if engine == Engine::Mongo => "null".into(),
        ParamValue::Null => "NULL".into(),
        ParamValue::Number { value } => {
            let value = value.trim();
            if !is_number(value) {
                return Err(format!("{value} is not a number."));
            }
            value.to_owned()
        }
        // JSON's own quoting, which the Mongo parser reads back exactly.
        ParamValue::Text { value } if engine == Engine::Mongo => {
            serde_json::Value::from(value.as_str()).to_string()
        }
        // Quotes doubled. A backslash is left as typed: the gate then reads
        // the whole statement as unreadable and asks before it runs.
        ParamValue::Text { value } => format!("'{}'", value.replace('\'', "''")),
    })
}

/// An optional minus, digits, an optional fraction and exponent: nothing that
/// could close the literal it stands in for.
fn is_number(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (digits, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, "0"));
    let all_digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    all_digits(whole)
        && all_digits(fraction)
        && exponent.is_none_or(|exponent| {
            all_digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent))
        })
}

/// The tokenizer's line and column (1-based, in chars) as byte offsets.
struct Bytes<'a> {
    lines: Vec<(&'a str, usize)>,
}

impl<'a> Bytes<'a> {
    fn new(text: &'a str) -> Self {
        let mut at = 0;
        let lines = text
            .split_inclusive('\n')
            .map(|line| {
                let start = at;
                at += line.len();
                (line, start)
            })
            .collect();
        Self { lines }
    }

    fn at(&self, location: Location) -> usize {
        let Some(&(line, start)) = self.lines.get(location.line.saturating_sub(1) as usize) else {
            return self
                .lines
                .last()
                .map_or(0, |(line, start)| start + line.len());
        };
        let column = location.column.saturating_sub(1) as usize;
        start + line.chars().take(column).map(char::len_utf8).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> ParamValue {
        ParamValue::Text {
            value: value.into(),
        }
    }

    fn bind(name: &str, value: ParamValue) -> Binding {
        Binding {
            name: name.into(),
            value,
        }
    }

    #[test]
    fn finds_only_real_placeholders() {
        let sql = "SELECT * FROM t WHERE a = :id AND b::text = ':no' -- :comment\nAND c = :name_2 OR d = :id";
        assert_eq!(parameters(Engine::Postgres, sql), ["id", "name_2"]);
        assert_eq!(
            parameters(Engine::MySql, "SELECT :x, ':y', `:z`, a[1:2]"),
            ["x"]
        );
        assert_eq!(
            parameters(Engine::Mongo, "db.t.find({a: :v, b:true, c: ':w'})"),
            ["v"]
        );
    }

    #[test]
    fn fills_with_literals() {
        let sql = "SELECT * FROM t WHERE name = :name AND n > :n AND x IS :none AND é = :name";
        let filled = fill(
            Engine::Postgres,
            sql,
            &[
                bind("name", text("O'Brien")),
                bind(
                    "n",
                    ParamValue::Number {
                        value: " -1.5e3 ".into(),
                    },
                ),
                bind("none", ParamValue::Null),
            ],
        )
        .unwrap();
        assert_eq!(
            filled,
            "SELECT * FROM t WHERE name = 'O''Brien' AND n > -1.5e3 AND x IS NULL AND é = 'O''Brien'"
        );
        let mongo = fill(
            Engine::Mongo,
            "db.t.find({name: :name})",
            &[bind("name", text("a\"b"))],
        )
        .unwrap();
        assert_eq!(mongo, r#"db.t.find({name: "a\"b"})"#);
        assert!(crate::mongo_cmd::parse(&mongo).is_ok());
    }

    #[test]
    fn a_value_cannot_become_a_statement() {
        // Not a number: refused before anything is built.
        let injected = ParamValue::Number {
            value: "1; DROP TABLE t".into(),
        };
        assert!(fill(Engine::MySql, "SELECT :n", &[bind("n", injected)]).is_err());
        // A string stays one string, and the gate reads the filled text.
        let filled = fill(
            Engine::MySql,
            "SELECT * FROM t WHERE a = :a",
            &[bind("a", text("x'; DROP TABLE t; -- "))],
        )
        .unwrap();
        assert_eq!(
            crate::gate::classify(Engine::MySql, &filled),
            crate::gate::Verdict::READ
        );
        // With a backslash, the server could read the quotes differently:
        // the gate then refuses to vouch for it.
        let sneaky = fill(
            Engine::MySql,
            "SELECT * FROM t WHERE a = :a",
            &[bind("a", text(r"x\'; DROP TABLE t; -- "))],
        )
        .unwrap();
        assert_eq!(
            crate::gate::classify(Engine::MySql, &sneaky),
            crate::gate::unreadable()
        );
        // Every parameter needs a value.
        assert!(
            fill(
                Engine::Postgres,
                "SELECT :a, :b",
                &[bind("a", ParamValue::Null)]
            )
            .is_err()
        );
    }
}
