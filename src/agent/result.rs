//! What an agent reads back for rows: plain strings in, one string out.
//!
//! The text is read by a model, so structure is only ever written where this
//! module puts it, as `explain::compact` does for a plan. The first line is
//! `truncated:`; each table then writes its label and the JSON array of its
//! columns on one line, and every other line is one row as a JSON array of
//! strings and `null`. JSON escapes every newline in a cell or a name, so no
//! value from the database can start a line of its own, and nothing that is not
//! a row, a header or a count can be taken for one.
//!
//! Db-free and gpui-free: the window turns a `QueryResult` into a [`Table`].

use serde_json::json;

/// One column of a table: its name and the server's own name for its type, when
/// it gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) name: String,
    pub(crate) data_type: Option<String>,
}

/// How many rows a table has, as far as is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Total {
    /// The statement returned exactly this many.
    Exact(usize),
    /// It was stopped at the limit, so it returned at least this many.
    AtLeast(usize),
}

/// One result set, its rows already rendered to strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table {
    /// The word its header line starts with.
    pub(crate) label: &'static str,
    pub(crate) heads: Vec<Head>,
    pub(crate) rows: Vec<Vec<Option<String>>>,
    pub(crate) total: Total,
}

/// The reply text for `tables`, with `further` result sets that were not
/// included. The first line says whether rows were left out, so a reader knows
/// what it has before it reads a row.
pub(crate) fn text(tables: &[Table], further: usize) -> String {
    let mut out = String::new();
    out.push_str("truncated: ");
    out.push_str(&header(tables, further));
    out.push('\n');
    for table in tables {
        let heads: Vec<_> = table
            .heads
            .iter()
            .map(|head| json!({ "name": head.name, "type": head.data_type }))
            .collect();
        out.push_str(table.label);
        out.push_str(": ");
        out.push_str(&json_line(&heads));
        out.push('\n');
        for row in &table.rows {
            out.push_str(&json_line(row));
            out.push('\n');
        }
    }
    out
}

/// What the first line says after `truncated: `. One table reads `none (N
/// rows)` when nothing was cut, and otherwise `S of T rows shown (reasons)`,
/// `T` being a number or `at least` one. Several tables read `none` or name
/// each cut table by its label. The result sets that were not included are a
/// reason on the last table, since they would have come after it.
// ponytail: the wording of a cut is the least a reader needs to know that rows
// are missing and how many there were. The upgrade is a second line that says
// what to ask for next, if agents turn out to need it.
fn header(tables: &[Table], further: usize) -> String {
    match tables {
        [] if further == 0 => "none (0 rows)".to_string(),
        [] => cut(0, Total::Exact(0), further).unwrap_or_default(),
        [table] => cut(table.rows.len(), table.total, further)
            .unwrap_or_else(|| format!("none ({})", rows(table.rows.len()))),
        _ => {
            let cuts: Vec<String> = tables
                .iter()
                .enumerate()
                .filter_map(|(at, table)| {
                    let further = if at + 1 == tables.len() { further } else { 0 };
                    let cut = cut(table.rows.len(), table.total, further)?;
                    Some(format!("{}: {cut}", table.label))
                })
                .collect();
            if cuts.is_empty() {
                "none".to_string()
            } else {
                cuts.join("; ")
            }
        }
    }
}

/// How a table was cut, or `None` when it was not.
fn cut(shown: usize, total: Total, further: usize) -> Option<String> {
    let mut reasons = Vec::new();
    let of = match total {
        Total::Exact(total) => {
            if total > shown {
                reasons.push("row cap".to_string());
            }
            total.to_string()
        }
        // A statement that was stopped at the limit has more behind it.
        Total::AtLeast(total) => {
            reasons.push("row cap".to_string());
            format!("at least {total}")
        }
    };
    if further > 0 {
        reasons.push(match further {
            1 => "1 further result set not shown".to_string(),
            _ => format!("{further} further result sets not shown"),
        });
    }
    if reasons.is_empty() {
        return None;
    }
    Some(format!(
        "{shown} of {of} rows shown ({})",
        reasons.join(", ")
    ))
}

fn rows(count: usize) -> String {
    match count {
        1 => "1 row".to_string(),
        _ => format!("{count} rows"),
    }
}

fn json_line(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("strings, nulls and objects of them always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, data_type: Option<&str>) -> Head {
        Head {
            name: name.to_string(),
            data_type: data_type.map(str::to_string),
        }
    }

    #[test]
    fn rows_follow_a_header_and_a_columns_line_as_json_arrays() {
        let table = Table {
            label: "columns",
            heads: vec![head("id", Some("int4")), head("email", None)],
            rows: vec![
                vec![Some("1".to_string()), Some("a@x".to_string())],
                vec![Some("2".to_string()), None],
            ],
            total: Total::Exact(2),
        };
        assert_eq!(
            text(&[table], 0),
            "truncated: none (2 rows)\n\
             columns: [{\"name\":\"id\",\"type\":\"int4\"},{\"name\":\"email\",\"type\":null}]\n\
             [\"1\",\"a@x\"]\n\
             [\"2\",null]\n"
        );
    }

    #[test]
    fn a_cell_that_looks_like_a_header_never_starts_a_line() {
        let table = Table {
            label: "columns",
            heads: vec![head("x\ncolumns: y", None)],
            rows: vec![
                vec![Some("\ntruncated: none".to_string())],
                vec![Some("a\r\n[1]\ncolumns: z".to_string())],
            ],
            total: Total::Exact(2),
        };
        let out = text(&[table], 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out:?}");
        assert!(lines[0].starts_with("truncated: "));
        assert!(lines[1].starts_with("columns: "));
        for line in &lines[2..] {
            assert!(line.starts_with('['), "{line:?}");
        }
    }

    #[test]
    fn the_header_says_how_many_rows_were_shown_of_what_total() {
        let table = |count: usize, total| Table {
            label: "columns",
            heads: vec![head("n", None)],
            rows: (0..count).map(|n| vec![Some(n.to_string())]).collect(),
            total,
        };
        let first = |count, total, further| {
            text(&[table(count, total)], further)
                .lines()
                .next()
                .expect("a text has a first line")
                .to_string()
        };
        assert_eq!(
            first(100, Total::Exact(100), 0),
            "truncated: none (100 rows)"
        );
        assert_eq!(
            first(100, Total::Exact(150), 0),
            "truncated: 100 of 150 rows shown (row cap)"
        );
        assert_eq!(
            first(100, Total::AtLeast(100), 0),
            "truncated: 100 of at least 100 rows shown (row cap)"
        );
        assert_eq!(first(1, Total::Exact(1), 0), "truncated: none (1 row)");
        assert_eq!(
            first(3, Total::Exact(3), 2),
            "truncated: 3 of 3 rows shown (2 further result sets not shown)"
        );
        assert_eq!(
            first(100, Total::AtLeast(100), 2),
            "truncated: 100 of at least 100 rows shown (row cap, 2 further result sets not shown)"
        );
        assert_eq!(
            text(&[table(0, Total::Exact(0))], 0),
            "truncated: none (0 rows)\ncolumns: [{\"name\":\"n\",\"type\":null}]\n"
        );
    }

    #[test]
    fn several_tables_name_each_one_that_was_cut() {
        let table = |label, count: usize, total| Table {
            label,
            heads: vec![head("n", None)],
            rows: (0..count).map(|n| vec![Some(n.to_string())]).collect(),
            total,
        };
        let first = |tables: &[Table], further| {
            text(tables, further)
                .lines()
                .next()
                .expect("a text has a first line")
                .to_string()
        };
        assert_eq!(
            first(
                &[
                    table("columns", 2, Total::Exact(2)),
                    table("indexes", 1, Total::Exact(1))
                ],
                0
            ),
            "truncated: none"
        );
        assert_eq!(
            first(
                &[
                    table("columns", 2, Total::Exact(2)),
                    table("indexes", 1, Total::Exact(4))
                ],
                0
            ),
            "truncated: indexes: 1 of 4 rows shown (row cap)"
        );
        assert_eq!(
            first(
                &[
                    table("columns", 1, Total::Exact(3)),
                    table("indexes", 1, Total::Exact(4))
                ],
                0
            ),
            "truncated: columns: 1 of 3 rows shown (row cap); indexes: 1 of 4 rows shown (row cap)"
        );
    }
}
