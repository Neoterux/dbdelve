//! MongoDB commands in the shell's spelling, and what each may do.
//!
//! One call per statement, `db.<collection>.<method>(<args>)`,
//! `db.getCollection('<name>').<method>(<args>)`, or one of the database's own
//! user methods (`db.createUser(…)`), with arguments in JSON5 (the shell's
//! unquoted keys and single quotes) holding Extended JSON for types.
// ponytail: no cursor chains (`.sort().limit()`), no `ObjectId("…")` helper
// calls (`{"$oid": "…"}` instead) and no JavaScript. The desktop's `mql`
// module is the upgrade path once it can be shared.

use bson::{Bson, Document};

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
    GetUsers,
    CreateUser {
        user: String,
        password: String,
        roles: Vec<Bson>,
    },
    SetPassword {
        user: String,
        password: String,
    },
    GrantRoles {
        user: String,
        roles: Vec<Bson>,
    },
    RevokeRoles {
        user: String,
        roles: Vec<Bson>,
    },
    DropUser {
        user: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    /// None for the database's own methods.
    pub collection: Option<String>,
    pub call: Call,
}

/// The database's methods, called as `db.<method>(…)`: users only.
const DATABASE_METHODS: &[&str] = &[
    "getUsers",
    "createUser",
    "updateUser",
    "grantRolesToUser",
    "revokeRolesFromUser",
    "dropUser",
];

pub fn parse(text: &str) -> Result<Command, String> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let rest = text
        .strip_prefix("db.")
        .ok_or("A MongoDB command starts with db.<collection>.")?;
    let database_method = DATABASE_METHODS.iter().find(|method| {
        rest.strip_prefix(**method)
            .is_some_and(|after| after.starts_with('('))
    });
    let (collection, rest) = match database_method {
        Some(_) => (None, rest),
        None => {
            let (collection, rest) = collection(rest)?;
            let rest = rest
                .strip_prefix('.')
                .ok_or("Expected .<method>(…) after the collection.")?;
            (Some(collection), rest)
        }
    };
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
    // Options this does not pass on (`upsert`, `arrayFilters`, a collation)
    // are refused, never dropped: dropping one runs a different command
    // from the one typed.
    let most = match method {
        "drop" | "getUsers" => 0,
        "find"
        | "updateOne"
        | "updateMany"
        | "updateUser"
        | "grantRolesToUser"
        | "revokeRolesFromUser" => 2,
        _ => 1,
    };
    if args.len() > most {
        return Err(format!(
            "{method} here takes {most} argument{}; options are not passed on, so a command \
             with them is refused rather than run without them.",
            if most == 1 { "" } else { "s" }
        ));
    }
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
        _ if collection.is_none() => database_call(method, &args)?,
        other => return Err(format!("{other} is not a method this app runs.")),
    };
    Ok(Command { collection, call })
}

fn database_call(method: &str, args: &[Bson]) -> Result<Call, String> {
    let name = |at: usize| match args.get(at) {
        Some(Bson::String(name)) if !name.is_empty() => Ok(name.clone()),
        _ => Err(format!(
            "{method} takes the user's name as argument {}.",
            at + 1
        )),
    };
    let roles = |at: usize| match args.get(at) {
        Some(Bson::Array(roles)) => Ok(roles.clone()),
        _ => Err(format!(
            "{method} takes an array of roles as argument {}.",
            at + 1
        )),
    };
    // Only the fields named here: a field this does not know could change
    // what the command does without the gate having read it.
    let only = |document: &Document, allowed: &[&str]| match document
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        Some(key) => Err(format!("{method} here takes no {key}.")),
        None => Ok(()),
    };
    let text = |document: &Document, key: &str| match document.get(key) {
        Some(Bson::String(value)) => Ok(value.clone()),
        _ => Err(format!("{method} needs {key} as a string.")),
    };
    Ok(match method {
        "getUsers" if args.is_empty() => Call::GetUsers,
        "createUser" => {
            let Some(Bson::Document(spec)) = args.first() else {
                return Err("createUser takes one document: {user, pwd, roles}.".into());
            };
            only(spec, &["user", "pwd", "roles"])?;
            Call::CreateUser {
                user: text(spec, "user")?,
                password: text(spec, "pwd")?,
                roles: match spec.get("roles") {
                    Some(Bson::Array(roles)) => roles.clone(),
                    _ => return Err("createUser needs roles, an array (it may be empty).".into()),
                },
            }
        }
        "updateUser" => {
            let Some(Bson::Document(update)) = args.get(1) else {
                return Err("updateUser takes the name and {pwd}.".into());
            };
            only(update, &["pwd"])?;
            Call::SetPassword {
                user: name(0)?,
                password: text(update, "pwd")?,
            }
        }
        "grantRolesToUser" => Call::GrantRoles {
            user: name(0)?,
            roles: roles(1)?,
        },
        "revokeRolesFromUser" => Call::RevokeRoles {
            user: name(0)?,
            roles: roles(1)?,
        },
        "dropUser" => Call::DropUser { user: name(0)? },
        _ => return Err(format!("{method} takes different arguments.")),
    })
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
        return unreadable();
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
        Call::GetUsers => Verdict::READ,
        // Who may do what is the server's own boundary: as `GRANT` is.
        Call::CreateUser { .. }
        | Call::SetPassword { .. }
        | Call::GrantRoles { .. }
        | Call::RevokeRoles { .. } => Verdict::FULL,
        Call::DropUser { .. } => Verdict::destroys(Destructive::Drop),
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
        assert_eq!(command.collection.as_deref(), Some("orders"));
        let Call::Find { filter, projection } = command.call else {
            panic!()
        };
        assert_eq!(filter.get_str("status").unwrap(), "open");
        assert!(projection.is_some());

        let command =
            parse("db.getCollection('a.b').aggregate([{$match: {}}, {$count: 'n'}])").unwrap();
        assert_eq!(command.collection.as_deref(), Some("a.b"));
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
    fn user_methods() {
        let create = parse(
            "db.createUser({user: 'app', pwd: 's3cret', roles: [{role: 'read', db: 'dev'}]})",
        )
        .unwrap();
        assert_eq!(create.collection, None);
        assert!(
            matches!(&create.call, Call::CreateUser { user, roles, .. } if user == "app" && roles.len() == 1)
        );
        assert_eq!(
            classify("db.createUser({user: 'a', pwd: 'b', roles: []})"),
            Verdict::FULL
        );
        assert_eq!(
            classify("db.grantRolesToUser('a', ['read'])"),
            Verdict::FULL
        );
        assert_eq!(classify("db.updateUser('a', {pwd: 'c'})"), Verdict::FULL);
        assert_eq!(
            classify("db.dropUser('a')"),
            Verdict::destroys(Destructive::Drop)
        );
        assert_eq!(classify("db.getUsers()"), Verdict::READ);
        // A field the gate has not read is refused, not passed along.
        assert!(parse("db.updateUser('a', {pwd: 'c', roles: ['root']})").is_err());
        // Options it would not pass on are refused, not dropped.
        assert!(parse("db.t.updateMany({}, {$set: {a: 1}}, {upsert: true})").is_err());
        assert!(parse("db.t.find({}, {}, {})").is_err());
        assert_eq!(
            classify("db.t.aggregate([], {allowDiskUse: true})"),
            unreadable()
        );
        // A collection that happens to share a method's name is still one.
        assert_eq!(
            parse("db.dropUser.find()").unwrap().collection.as_deref(),
            Some("dropUser")
        );
    }

    #[test]
    fn collection_refs_read_back() {
        assert_eq!(collection_ref("orders"), "db.orders");
        assert_eq!(collection_ref("a.b"), r#"db.getCollection("a.b")"#);
        assert_eq!(
            parse(&format!("{}.find()", collection_ref("a.b")))
                .unwrap()
                .collection
                .as_deref(),
            Some("a.b")
        );
    }
}
