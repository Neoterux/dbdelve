//! MongoDB commands in the shell's spelling, and what each may do.
//!
//! One call per statement, `db.<collection>.<method>(<args>)` or
//! `db.getCollection('<name>').<method>(<args>)`, with arguments in JSON5 (the
//! shell's unquoted keys and single quotes) holding Extended JSON for types.
// ponytail: no cursor chains (`.sort().limit()`), no `ObjectId("…")` helper
// calls (`{"$oid": "…"}` instead) and no JavaScript. The desktop's `mql`
// module is the upgrade path once it can be shared.

use bson::{Bson, Document};

use crate::Engine;
use crate::gate::{Destructive, Verdict, unreadable};

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Find {
        filter: Document,
        projection: Option<Document>,
    },
    Aggregate {
        pipeline: Vec<Document>,
    },
    Count {
        filter: Document,
    },
    InsertOne {
        document: Document,
    },
    InsertMany {
        documents: Vec<Document>,
    },
    UpdateOne {
        filter: Document,
        update: Document,
    },
    UpdateMany {
        filter: Document,
        update: Document,
    },
    DeleteOne {
        filter: Document,
    },
    DeleteMany {
        filter: Document,
    },
    Drop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub collection: String,
    pub call: Call,
}

pub fn parse(text: &str) -> Result<Command, String> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let rest = text
        .strip_prefix("db.")
        .ok_or("A MongoDB command starts with db.<collection>.")?;
    let (collection, rest) = collection(rest)?;
    let rest = rest
        .strip_prefix('.')
        .ok_or("Expected .<method>(…) after the collection.")?;
    let open = rest
        .find('(')
        .ok_or("Expected (…) after the method name.")?;
    let method = rest[..open].trim();
    let args = rest[open + 1..]
        .strip_suffix(')')
        .ok_or("The command must end with the method's closing parenthesis.")?;
    let args: Vec<serde_json::Value> = json5::from_str(&format!("[{args}]"))
        .map_err(|error| format!("Arguments are not valid JSON5: {error}"))?;
    let args = args
        .into_iter()
        .map(|value| Bson::try_from(value).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let document = |at: usize| -> Result<Document, String> {
        match args.get(at) {
            None => Ok(Document::new()),
            Some(Bson::Document(document)) => Ok(document.clone()),
            Some(_) => Err(format!("{method} takes a document as argument {}.", at + 1)),
        }
    };
    let required = |at: usize| -> Result<Document, String> {
        if args.len() <= at {
            return Err(format!("{method} needs a document as argument {}.", at + 1));
        }
        document(at)
    };
    let documents = || -> Result<Vec<Document>, String> {
        match args.first() {
            Some(Bson::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Bson::Document(document) => Ok(document.clone()),
                    _ => Err(format!("Every item {method} takes is a document.")),
                })
                .collect(),
            _ => Err(format!("{method} takes an array of documents.")),
        }
    };
    let call = match method {
        "find" => Call::Find {
            filter: document(0)?,
            projection: args.get(1).map(|_| document(1)).transpose()?,
        },
        "countDocuments" => Call::Count {
            filter: document(0)?,
        },
        "aggregate" => Call::Aggregate {
            pipeline: documents()?,
        },
        "insertOne" => Call::InsertOne {
            document: required(0)?,
        },
        "insertMany" => Call::InsertMany {
            documents: documents()?,
        },
        "updateOne" => Call::UpdateOne {
            filter: required(0)?,
            update: required(1)?,
        },
        "updateMany" => Call::UpdateMany {
            filter: required(0)?,
            update: required(1)?,
        },
        "deleteOne" => Call::DeleteOne {
            filter: required(0)?,
        },
        "deleteMany" => Call::DeleteMany {
            filter: required(0)?,
        },
        "drop" => Call::Drop,
        other => return Err(format!("{other} is not a method this app runs.")),
    };
    Ok(Command { collection, call })
}

fn collection(rest: &str) -> Result<(String, &str), String> {
    if let Some(after) = rest.strip_prefix("getCollection(") {
        let close = after.find(')').ok_or("getCollection( is not closed.")?;
        let name: String = json5::from_str(&after[..close])
            .map_err(|_| "getCollection takes the collection's name as a string.")?;
        return Ok((name, &after[close + 1..]));
    }
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '$'))
        .unwrap_or(rest.len());
    if end == 0 {
        return Err("Expected a collection name after db.".into());
    }
    Ok((rest[..end].to_owned(), &rest[end..]))
}

/// How a generated read names a collection: bare where the shell would read it
/// back as the same name, else through `getCollection`.
pub fn collection_ref(name: &str) -> String {
    let bare = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if bare {
        format!("db.{name}")
    } else {
        format!("db.getCollection({})", serde_json::Value::from(name))
    }
}

/// Operators that run JavaScript on the server.
fn runs_javascript(document: &Document) -> bool {
    fn walk(value: &Bson) -> bool {
        match value {
            Bson::Document(document) => document.iter().any(|(key, value)| {
                matches!(key.as_str(), "$where" | "$function" | "$accumulator") || walk(value)
            }),
            Bson::Array(items) => items.iter().any(walk),
            Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => true,
            _ => false,
        }
    }
    walk(&Bson::Document(document.clone()))
}

pub fn classify(text: &str) -> Verdict {
    let Ok(command) = parse(text) else {
        return unreadable(Engine::Mongo);
    };
    let script =
        |documents: &[&Document]| documents.iter().any(|document| runs_javascript(document));
    match &command.call {
        Call::Find { filter, projection } => {
            let mut documents = vec![filter];
            documents.extend(projection);
            if script(&documents) {
                Verdict::FULL
            } else {
                Verdict::READ
            }
        }
        Call::Count { filter } => {
            if script(&[filter]) {
                Verdict::FULL
            } else {
                Verdict::READ
            }
        }
        Call::Aggregate { pipeline } => pipeline.iter().fold(Verdict::READ, |verdict, stage| {
            verdict.max(if stage.contains_key("$out") {
                // `$out` replaces the whole target collection.
                Verdict::destroys(Destructive::Drop)
            } else if stage.contains_key("$merge") {
                Verdict::WRITE
            } else if runs_javascript(stage) {
                Verdict::FULL
            } else {
                Verdict::READ
            })
        }),
        Call::InsertOne { .. } | Call::InsertMany { .. } => Verdict::WRITE,
        Call::UpdateOne { filter, update } | Call::UpdateMany { filter, update } => {
            if script(&[filter, update]) {
                Verdict::FULL
            } else {
                Verdict::WRITE
            }
        }
        Call::DeleteOne { filter } => {
            if script(&[filter]) {
                Verdict::FULL
            } else {
                Verdict::WRITE
            }
        }
        Call::DeleteMany { filter } if filter.is_empty() => {
            Verdict::destroys(Destructive::UnfilteredDelete)
        }
        Call::DeleteMany { filter } => {
            if script(&[filter]) {
                Verdict::FULL
            } else {
                Verdict::WRITE
            }
        }
        Call::Drop => Verdict::destroys(Destructive::Drop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Mode;

    #[test]
    fn parses_the_shell_spelling() {
        let command =
            parse("db.orders.find({status: 'open', total: {$gt: 10}}, {_id: 0});").unwrap();
        assert_eq!(command.collection, "orders");
        let Call::Find { filter, projection } = command.call else {
            panic!()
        };
        assert_eq!(filter.get_str("status").unwrap(), "open");
        assert!(projection.is_some());

        let command =
            parse("db.getCollection('a.b').aggregate([{$match: {}}, {$count: 'n'}])").unwrap();
        assert_eq!(command.collection, "a.b");
        assert!(matches!(command.call, Call::Aggregate { pipeline } if pipeline.len() == 2));

        let oid = parse(r#"db.x.find({_id: {"$oid": "65a1f0c2e4b0a1b2c3d4e5f6"}})"#).unwrap();
        let Call::Find { filter, .. } = oid.call else {
            panic!()
        };
        assert!(filter.get_object_id("_id").is_ok());
    }

    #[test]
    fn verdicts() {
        assert_eq!(classify("db.t.find()"), Verdict::READ);
        assert_eq!(classify("db.t.find({$where: 'sleep(1)'})"), Verdict::FULL);
        assert_eq!(classify("db.t.insertOne({a: 1})"), Verdict::WRITE);
        assert_eq!(
            classify("db.t.deleteMany({})"),
            Verdict::destroys(Destructive::UnfilteredDelete)
        );
        assert_eq!(classify("db.t.deleteMany({a: 1})"), Verdict::WRITE);
        assert_eq!(
            classify("db.t.drop()"),
            Verdict::destroys(Destructive::Drop)
        );
        assert_eq!(
            classify("db.t.aggregate([{$match: {}}, {$out: 'u'}])"),
            Verdict::destroys(Destructive::Drop)
        );
        assert_eq!(
            classify("db.t.aggregate([{$merge: {into: 'u'}}])"),
            Verdict::WRITE
        );
        let typo = classify("db.t.fnd()");
        assert_eq!(typo.mode, Mode::ReadWrite);
        assert_eq!(typo.destructive, vec![Destructive::Unreadable]);
    }

    #[test]
    fn collection_refs_read_back() {
        assert_eq!(collection_ref("orders"), "db.orders");
        assert_eq!(collection_ref("a.b"), r#"db.getCollection("a.b")"#);
        assert_eq!(
            parse(&format!("{}.find()", collection_ref("a.b")))
                .unwrap()
                .collection,
            "a.b"
        );
    }
}
