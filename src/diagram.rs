//! The diagram the Show DDL dialog toggles to: a table, its columns with
//! their keys, and the tables its foreign keys point to, built from the
//! [`Structure`] a structure tab already reads. Pure: data in, a value out.
//! The targets come from the foreign keys alone, so nothing is read per target
//! and a target lists only the columns that are pointed at.

use crate::db::Structure;

/// A table and the tables its foreign keys point to, in the order the keys
/// first name them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Diagram {
    pub(crate) table: DiagramTable,
    pub(crate) targets: Vec<DiagramTable>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiagramTable {
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) columns: Vec<DiagramColumn>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiagramColumn {
    pub(crate) name: String,
    /// `None` for a column of a target, which is only named by the key.
    pub(crate) data_type: Option<String>,
    pub(crate) primary_key: bool,
    /// The index into [`Diagram::targets`] of the table this column's foreign
    /// key points to.
    pub(crate) target: Option<usize>,
}

/// The diagram of `schema.name` as `structure` describes it. Targets are
/// grouped by (schema, table) in the order the foreign keys first name them,
/// each listing a referenced column once however many keys point at it.
pub(crate) fn of(schema: &str, name: &str, structure: &Structure) -> Diagram {
    let mut targets: Vec<DiagramTable> = Vec::new();
    let mut target_of_key = Vec::with_capacity(structure.foreign_keys.len());
    for key in &structure.foreign_keys {
        let at = match targets.iter().position(|target| {
            target.schema == key.referenced_schema && target.name == key.referenced_table
        }) {
            Some(at) => at,
            None => {
                targets.push(DiagramTable {
                    schema: key.referenced_schema.clone(),
                    name: key.referenced_table.clone(),
                    columns: Vec::new(),
                });
                targets.len() - 1
            }
        };
        let columns = &mut targets[at].columns;
        if !columns
            .iter()
            .any(|column| column.name == key.referenced_column)
        {
            columns.push(DiagramColumn {
                name: key.referenced_column.clone(),
                data_type: None,
                primary_key: false,
                target: None,
            });
        }
        target_of_key.push(at);
    }

    let primary = structure.primary_key();
    let columns = structure
        .columns
        .iter()
        .map(|column| DiagramColumn {
            name: column.name.clone(),
            data_type: Some(column.data_type.clone()).filter(|data_type| !data_type.is_empty()),
            primary_key: primary.contains(&column.name),
            target: structure
                .foreign_keys
                .iter()
                .position(|key| key.column == column.name)
                .map(|key| target_of_key[key]),
        })
        .collect();
    Diagram {
        table: DiagramTable {
            schema: schema.to_string(),
            name: name.to_string(),
            columns,
        },
        targets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, ForeignKey, NamedDefinition};

    fn column(name: &str) -> ColumnDefinition {
        ColumnDefinition {
            name: name.into(),
            data_type: "integer".into(),
            nullable: true,
            default: None,
        }
    }

    fn key(column: &str, schema: &str, table: &str, referenced: &str) -> ForeignKey {
        ForeignKey {
            column: column.into(),
            referenced_schema: schema.into(),
            referenced_table: table.into(),
            referenced_column: referenced.into(),
        }
    }

    fn primary(definition: &str) -> NamedDefinition {
        NamedDefinition {
            name: "pk".into(),
            definition: definition.into(),
        }
    }

    fn names(table: &DiagramTable) -> Vec<&str> {
        table.columns.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn a_table_draws_its_columns_keys_and_the_tables_its_foreign_keys_point_to() {
        let structure = Structure {
            columns: vec![column("id"), column("owner_id"), column("kind_id")],
            constraints: vec![primary("PRIMARY KEY (id)")],
            foreign_keys: vec![
                key("owner_id", "main", "owners", "id"),
                key("kind_id", "main", "kinds", "code"),
            ],
            ..Structure::default()
        };
        let diagram = of("main", "birds", &structure);
        assert_eq!(
            (&*diagram.table.schema, &*diagram.table.name),
            ("main", "birds")
        );
        assert_eq!(names(&diagram.table), ["id", "owner_id", "kind_id"]);
        let marks: Vec<_> = diagram
            .table
            .columns
            .iter()
            .map(|c| (c.primary_key, c.target))
            .collect();
        assert_eq!(marks, [(true, None), (false, Some(0)), (false, Some(1))]);
        assert_eq!(
            diagram.table.columns[0].data_type.as_deref(),
            Some("integer")
        );
        assert_eq!(diagram.targets.len(), 2);
        assert_eq!(diagram.targets[0].name, "owners");
        assert_eq!(names(&diagram.targets[0]), ["id"]);
        assert_eq!(diagram.targets[1].name, "kinds");
        assert_eq!(names(&diagram.targets[1]), ["code"]);
    }

    #[test]
    fn two_keys_to_one_table_share_its_box() {
        let structure = Structure {
            columns: vec![column("a"), column("b")],
            foreign_keys: vec![key("a", "main", "t", "x"), key("b", "main", "t", "y")],
            ..Structure::default()
        };
        let diagram = of("main", "s", &structure);
        assert_eq!(diagram.targets.len(), 1);
        assert_eq!(diagram.targets[0].name, "t");
        assert_eq!(names(&diagram.targets[0]), ["x", "y"]);
        assert_eq!(
            diagram
                .table
                .columns
                .iter()
                .map(|c| c.target)
                .collect::<Vec<_>>(),
            [Some(0), Some(0)]
        );
    }

    #[test]
    fn a_composite_primary_key_marks_each_of_its_columns() {
        let structure = Structure {
            columns: vec![column("a"), column("b"), column("c")],
            constraints: vec![primary("PRIMARY KEY (\"a\", \"b\")")],
            ..Structure::default()
        };
        let diagram = of("main", "t", &structure);
        let marked: Vec<_> = diagram
            .table
            .columns
            .iter()
            .map(|c| c.primary_key)
            .collect();
        assert_eq!(marked, [true, true, false]);
    }
}
