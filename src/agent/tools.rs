//! The agent's tool list. Static: it never varies with what is visible or what
//! state the app is in, because a list that changed under the agent would be a
//! tool the agent had approved turning into another. Only results vary.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A tool the agent can call, spelled on the wire as its MCP name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tool {
    ListConnections,
    /// Lists a connection's schemas from its catalog.
    ListSchemas,
    /// Lists one schema's relations from the connection's catalog.
    ListTables,
    /// Describes one relation the catalog holds: columns, keys and indexes.
    DescribeTable,
    /// Runs one statement the gate lets an agent run without asking.
    ExecuteSql,
    /// Asks the database for the plan of one statement, without running it
    /// unless asked to measure it.
    ExplainQuery,
    /// Asks the person at the window to answer: nothing runs for it, so it
    /// only ever waits to be answered, to expire, or to be hung up.
    Ping,
}

impl Tool {
    /// Every tool, in the order the agent is shown them.
    pub(crate) const ALL: [Tool; 7] = [
        Tool::ListConnections,
        Tool::ListSchemas,
        Tool::ListTables,
        Tool::DescribeTable,
        Tool::ExecuteSql,
        Tool::ExplainQuery,
        Tool::Ping,
    ];

    /// The name the agent calls it by: the spelling serde writes for it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tool::ListConnections => "list_connections",
            Tool::ListSchemas => "list_schemas",
            Tool::ListTables => "list_tables",
            Tool::DescribeTable => "describe_table",
            Tool::ExecuteSql => "execute_sql",
            Tool::ExplainQuery => "explain_query",
            Tool::Ping => "ping",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Tool::ListConnections => {
                "Lists the DBDelve connections the user has made visible to agents, by name and engine."
            }
            Tool::ListSchemas => "Lists the schemas of a named connection, in name order.",
            Tool::ListTables => {
                "Lists the tables and views of one schema of a named connection, with each one's kind, its parent when it is a partition, and its estimated row count."
            }
            Tool::DescribeTable => {
                "Describes one table or view of a named connection: its columns with type, nullability and default, its primary key, its foreign keys and its indexes."
            }
            Tool::ExecuteSql => {
                "Runs one read statement on a named connection exactly as written and returns the first rows, header first. A statement that would change data is not run."
            }
            Tool::ExplainQuery => {
                "Returns the plan the database would use for one statement of a named connection, findings first and then the plan tree. With `analyze` true it runs the statement to measure it, which only a read may do; without it nothing is run."
            }
            Tool::Ping => {
                "Asks the user, in the DBDelve window, to answer a test from the agent for one connection. Waits until the user answers. Opens no database connection and runs nothing."
            }
        }
    }

    /// The JSON Schema of the arguments the tool takes. None takes more
    /// than it names, so an argument it does not know is refused up front
    /// rather than ignored.
    pub(crate) fn input_schema(self) -> Map<String, Value> {
        let mut schema = Map::new();
        schema.insert("type".to_string(), json!("object"));
        let properties = match self {
            Tool::ListConnections => json!({}),
            Tool::ListSchemas => {
                schema.insert("required".to_string(), json!(["connection"]));
                json!({ "connection": { "type": "string" } })
            }
            Tool::ListTables => {
                schema.insert("required".to_string(), json!(["connection", "schema"]));
                json!({ "connection": { "type": "string" }, "schema": { "type": "string" } })
            }
            Tool::DescribeTable => {
                schema.insert(
                    "required".to_string(),
                    json!(["connection", "schema", "table"]),
                );
                json!({
                    "connection": { "type": "string" },
                    "schema": { "type": "string" },
                    "table": { "type": "string" },
                })
            }
            Tool::ExecuteSql => {
                schema.insert("required".to_string(), json!(["connection", "sql"]));
                json!({ "connection": { "type": "string" }, "sql": { "type": "string" } })
            }
            Tool::ExplainQuery => {
                schema.insert("required".to_string(), json!(["connection", "sql"]));
                json!({
                    "connection": { "type": "string" },
                    "sql": { "type": "string" },
                    "analyze": { "type": "boolean" },
                })
            }
            Tool::Ping => {
                schema.insert("required".to_string(), json!(["connection"]));
                json!({ "connection": { "type": "string" } })
            }
        };
        schema.insert("properties".to_string(), properties);
        schema.insert("additionalProperties".to_string(), json!(false));
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_spelled_the_same_in_its_name_and_on_the_wire() {
        for tool in Tool::ALL {
            assert_eq!(serde_json::to_value(tool).unwrap(), json!(tool.name()));
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
        assert_eq!(Tool::from_name("List_Connections"), None);
    }

    #[test]
    fn no_tool_takes_an_argument_it_does_not_name() {
        for tool in Tool::ALL {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
        }
        assert_eq!(Tool::Ping.input_schema()["required"], json!(["connection"]));
        assert_eq!(Tool::ALL.len(), 7);
        assert_eq!(
            Tool::ListSchemas.input_schema()["required"],
            json!(["connection"])
        );
        let schema = Tool::ListTables.input_schema();
        assert_eq!(schema["required"], json!(["connection", "schema"]));
        assert_eq!(schema["properties"]["schema"], json!({"type": "string"}));
        let schema = Tool::DescribeTable.input_schema();
        assert_eq!(schema["required"], json!(["connection", "schema", "table"]));
        assert_eq!(schema["properties"]["table"], json!({"type": "string"}));
        let schema = Tool::ExecuteSql.input_schema();
        assert_eq!(schema["required"], json!(["connection", "sql"]));
        assert_eq!(
            schema["properties"]["connection"],
            json!({"type": "string"})
        );
        assert_eq!(schema["properties"]["sql"], json!({"type": "string"}));
        let schema = Tool::ExplainQuery.input_schema();
        assert_eq!(schema["required"], json!(["connection", "sql"]));
        assert_eq!(schema["properties"]["sql"], json!({"type": "string"}));
        assert_eq!(schema["properties"]["analyze"], json!({"type": "boolean"}));
    }
}
