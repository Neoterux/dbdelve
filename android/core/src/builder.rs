//! A query built by tapping instead of typing: a table, its columns, filters,
//! a sort and a limit, written out in the engine's own spelling.
//!
//! The builder writes structure only. Every value the user types becomes a
//! `:name` parameter, which `params::fill` turns into a literal, so nothing
//! typed into a filter is ever spliced into the statement here. What it
//! writes lands in a tab the user can read, and runs through the gate like
//! anything typed.

use crate::Engine;
use crate::mongo_cmd;

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FilterOp {
    Equals,
    NotEquals,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    /// SQL's `LIKE`; a regular expression on MongoDB.
    Like,
    IsNull,
    NotNull,
}

impl FilterOp {
    /// Whether the filter compares against a value the user types.
    fn takes_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::NotNull)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct QuerySpec {
    pub schema: String,
    pub table: String,
    /// Empty means every column.
    pub columns: Vec<String>,
    pub filters: Vec<Filter>,
    pub order_by: Option<String>,
    pub descending: bool,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BuiltQuery {
    pub text: String,
    /// The parameter each filter in [`QuerySpec::filters`] reads, in order;
    /// empty for a filter that takes no value.
    pub parameters: Vec<String>,
}

pub fn build(engine: Engine, spec: &QuerySpec) -> BuiltQuery {
    let mut taken: Vec<String> = Vec::new();
    let parameters: Vec<String> = spec
        .filters
        .iter()
        .map(|filter| {
            if !filter.op.takes_value() {
                return String::new();
            }
            let name = parameter_name(&filter.column, &taken);
            taken.push(name.clone());
            name
        })
        .collect();
    let text = match engine {
        Engine::Postgres => sql(spec, &parameters, '"'),
        Engine::MySql => sql(spec, &parameters, '`'),
        Engine::Mongo => mongo(spec, &parameters),
    };
    BuiltQuery { text, parameters }
}

fn sql(spec: &QuerySpec, parameters: &[String], mark: char) -> String {
    let quote = |name: &str| {
        let doubled = name.replace(mark, &format!("{mark}{mark}"));
        format!("{mark}{doubled}{mark}")
    };
    let columns = if spec.columns.is_empty() {
        "*".to_owned()
    } else {
        spec.columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let table = if spec.schema.is_empty() {
        quote(&spec.table)
    } else {
        format!("{}.{}", quote(&spec.schema), quote(&spec.table))
    };
    let mut text = format!("SELECT {columns}\nFROM {table}");
    let conditions: Vec<String> = spec
        .filters
        .iter()
        .zip(parameters)
        .map(|(filter, param)| {
            let column = quote(&filter.column);
            match filter.op {
                FilterOp::Equals => format!("{column} = :{param}"),
                FilterOp::NotEquals => format!("{column} <> :{param}"),
                FilterOp::Less => format!("{column} < :{param}"),
                FilterOp::LessOrEqual => format!("{column} <= :{param}"),
                FilterOp::Greater => format!("{column} > :{param}"),
                FilterOp::GreaterOrEqual => format!("{column} >= :{param}"),
                FilterOp::Like => format!("{column} LIKE :{param}"),
                FilterOp::IsNull => format!("{column} IS NULL"),
                FilterOp::NotNull => format!("{column} IS NOT NULL"),
            }
        })
        .collect();
    if !conditions.is_empty() {
        text.push_str("\nWHERE ");
        text.push_str(&conditions.join("\n  AND "));
    }
    if let Some(column) = &spec.order_by {
        text.push_str(&format!(
            "\nORDER BY {}{}",
            quote(column),
            if spec.descending { " DESC" } else { "" }
        ));
    }
    if let Some(limit) = spec.limit {
        text.push_str(&format!("\nLIMIT {limit}"));
    }
    text
}

// ponytail: a collection or field whose name holds `"` or `\` is written with
// JSON's backslash escape, which the SQL tokenizer `params` finds `:name`s
// with does not read; its values are then left unfilled, the command does not
// parse and the gate asks. A shared MongoDB-aware tokenizer is the fix.
/// `find` while that says it all; a pipeline once there is a sort or a
/// limit, since this app's MongoDB commands take no cursor chains.
fn mongo(spec: &QuerySpec, parameters: &[String]) -> String {
    let key = |name: &str| serde_json::Value::from(name).to_string();
    let conditions: Vec<String> = spec
        .filters
        .iter()
        .zip(parameters)
        .map(|(filter, param)| {
            let value = match filter.op {
                FilterOp::Equals => format!(":{param}"),
                FilterOp::NotEquals => format!("{{\"$ne\": :{param}}}"),
                FilterOp::Less => format!("{{\"$lt\": :{param}}}"),
                FilterOp::LessOrEqual => format!("{{\"$lte\": :{param}}}"),
                FilterOp::Greater => format!("{{\"$gt\": :{param}}}"),
                FilterOp::GreaterOrEqual => format!("{{\"$gte\": :{param}}}"),
                FilterOp::Like => format!("{{\"$regex\": :{param}}}"),
                FilterOp::IsNull => "null".to_owned(),
                FilterOp::NotNull => "{\"$ne\": null}".to_owned(),
            };
            format!("{}: {value}", key(&filter.column))
        })
        .collect();
    let filter = format!("{{{}}}", conditions.join(", "));
    let projection = (!spec.columns.is_empty()).then(|| {
        let fields: Vec<String> = spec
            .columns
            .iter()
            .map(|c| format!("{}: 1", key(c)))
            .collect();
        format!("{{{}}}", fields.join(", "))
    });
    let collection = mongo_cmd::collection_ref(&spec.table);
    if spec.order_by.is_none() && spec.limit.is_none() {
        return match projection {
            Some(projection) => format!("{collection}.find({filter}, {projection})"),
            None => format!("{collection}.find({filter})"),
        };
    }
    let mut stages = vec![format!("{{\"$match\": {filter}}}")];
    if let Some(column) = &spec.order_by {
        stages.push(format!(
            "{{\"$sort\": {{{}: {}}}}}",
            key(column),
            if spec.descending { -1 } else { 1 }
        ));
    }
    if let Some(limit) = spec.limit {
        stages.push(format!("{{\"$limit\": {limit}}}"));
    }
    if let Some(projection) = projection {
        stages.push(format!("{{\"$project\": {projection}}}"));
    }
    format!("{collection}.aggregate([\n  {}\n])", stages.join(",\n  "))
}

/// A parameter named after its column where the column's name can be one,
/// unique among those already [`taken`].
fn parameter_name(column: &str, taken: &[String]) -> String {
    let mut base: String = column
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    base = base.trim_matches('_').to_owned();
    if !base.starts_with(|c: char| c.is_ascii_alphabetic()) {
        base = format!("p_{base}").trim_end_matches('_').to_owned();
    }
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|name| !taken.contains(name))
        .expect("an unbounded range has a free name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{Mode, classify};
    use crate::params::{self, Binding, ParamValue};

    fn spec(filters: Vec<Filter>) -> QuerySpec {
        QuerySpec {
            schema: "public".into(),
            table: "order\"s".into(),
            columns: vec!["id".into(), "total".into()],
            filters,
            order_by: Some("id".into()),
            descending: true,
            limit: Some(50),
        }
    }

    fn filter(column: &str, op: FilterOp) -> Filter {
        Filter {
            column: column.into(),
            op,
        }
    }

    #[test]
    fn writes_values_only_as_parameters() {
        let built = build(
            Engine::Postgres,
            &spec(vec![
                filter("status", FilterOp::Equals),
                filter("status", FilterOp::NotEquals),
                filter("deleted at", FilterOp::IsNull),
                filter("1st", FilterOp::Greater),
            ]),
        );
        assert_eq!(built.parameters, ["status", "status_2", "", "p_1st"]);
        assert_eq!(
            built.text,
            "SELECT \"id\", \"total\"\nFROM \"public\".\"order\"\"s\"\n\
             WHERE \"status\" = :status\n  AND \"status\" <> :status_2\n  \
             AND \"deleted at\" IS NULL\n  AND \"1st\" > :p_1st\n\
             ORDER BY \"id\" DESC\nLIMIT 50"
        );
        assert_eq!(
            params::parameters(Engine::Postgres, &built.text),
            ["status", "status_2", "p_1st"]
        );
    }

    #[test]
    fn a_filled_value_cannot_escape_its_literal() {
        let built = build(Engine::MySql, &spec(vec![filter("name", FilterOp::Like)]));
        let filled = params::fill(
            Engine::MySql,
            &built.text,
            &[Binding {
                name: "name".into(),
                value: ParamValue::Text {
                    value: "x'; DROP TABLE t; --".into(),
                },
            }],
        )
        .unwrap();
        assert!(filled.contains("`name` LIKE 'x''; DROP TABLE t; --'"));
        assert_eq!(classify(Engine::MySql, &filled).mode, Mode::ReadOnly);
    }

    #[test]
    fn mongo_reads_back_through_the_shell_parser() {
        let plain = QuerySpec {
            table: "order items".into(),
            order_by: None,
            limit: None,
            columns: Vec::new(),
            ..spec(vec![filter("age", FilterOp::Greater)])
        };
        let built = build(Engine::Mongo, &plain);
        assert_eq!(
            built.text,
            "db.getCollection(\"order items\").find({\"age\": {\"$gt\": :age}})"
        );
        let sorted = build(
            Engine::Mongo,
            &QuerySpec {
                table: "orders".into(),
                ..spec(vec![
                    filter("tag", FilterOp::NotNull),
                    filter("n", FilterOp::Less),
                ])
            },
        );
        for text in [built.text, sorted.text] {
            let names = params::parameters(Engine::Mongo, &text);
            let bindings: Vec<Binding> = names
                .iter()
                .map(|name| Binding {
                    name: name.clone(),
                    value: ParamValue::Number { value: "3".into() },
                })
                .collect();
            let filled = params::fill(Engine::Mongo, &text, &bindings).unwrap();
            assert!(mongo_cmd::parse(&filled).is_ok(), "{filled}");
            assert_eq!(
                classify(Engine::Mongo, &filled).mode,
                Mode::ReadOnly,
                "{filled}"
            );
        }
    }
}
