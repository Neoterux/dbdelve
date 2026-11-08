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
    out.push_str(&match tables.first() {
        Some(table) => cut(table),
        None => "none (0 rows)".to_string(),
    });
    if further > 0 {
        out.push_str(&format!("; {further} further result sets are not shown"));
    }
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

// ponytail: the wording of a cut is the least a reader needs to know that rows
// are missing and how many there were. The upgrade is a second line that says
// what to ask for next, if agents turn out to need it.
fn cut(table: &Table) -> String {
    let shown = table.rows.len();
    match table.total {
        Total::Exact(total) if total == shown => format!("none ({})", rows(total)),
        Total::Exact(total) => format!("yes (the first {shown} of {})", rows(total)),
        Total::AtLeast(total) => {
            format!("yes (the first {shown} of at least {})", rows(total))
        }
    }
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
    fn a_cut_table_says_how_many_rows_it_kept_and_how_many_there_were() {
        let table = |total| Table {
            label: "columns",
            heads: vec![head("n", None)],
            rows: vec![vec![Some("1".to_string())]],
            total,
        };
        let first = |total, further| {
            text(&[table(total)], further)
                .lines()
                .next()
                .expect("a text has a first line")
                .to_string()
        };
        assert_eq!(
            first(Total::Exact(5), 0),
            "truncated: yes (the first 1 of 5 rows)"
        );
        assert_eq!(
            first(Total::AtLeast(1), 0),
            "truncated: yes (the first 1 of at least 1 row)"
        );
        assert_eq!(
            first(Total::Exact(1), 2),
            "truncated: none (1 row); 2 further result sets are not shown"
        );
    }
}
