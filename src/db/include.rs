//! A profile's include lists: which databases of a server the connections tree
//! and the agent lane show.
//!
//! A list is exact names and `*`/`?` globs, matched here in Rust against the
//! names the server listed. Nothing a person typed ever reaches a statement:
//! only a server-listed name that matched can, and an entry is never turned
//! into `LIKE`. An empty list shows everything. The connection's own database
//! is always shown, whatever the list says, so a filter cannot hide the place
//! the session is in.
//!
//! The lists are narrowed where an answer is read ([`Connection::databases`]
//! takes a [`Shown`]), so nothing stores an unnarrowed answer.
//!
//! [`Connection::databases`]: super::Connection::databases

use super::{Databases, Levels};

/// How many entries a list keeps. A file is a person's to edit: no more is
/// read than a profile would write.
pub const ENTRIES_CAP: usize = 500;

/// What a profile's lists say, as written in `profiles.toml` and in the form.
/// Not part of the connection: changing it never reconnects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Include {
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
}

impl Include {
    /// What a file held: entries that are empty or only whitespace dropped,
    /// at most [`ENTRIES_CAP`] of each list kept, in the order they were
    /// written.
    pub fn from_stored(databases: Vec<String>, schemas: Vec<String>) -> Self {
        let kept = |list: Vec<String>| {
            list.into_iter()
                .filter(|entry| !entry.trim().is_empty())
                .take(ENTRIES_CAP)
                .collect()
        };
        Self {
            databases: kept(databases),
            schemas: kept(schemas),
        }
    }

    /// The lists as they apply to a target of shape `levels` whose session is
    /// on `own_database`. A list the shape does not use is kept on disk and
    /// ignored here, and a shape with no databases to choose between is never
    /// filtered.
    pub fn resolve(&self, levels: Levels, own_database: Option<&str>) -> Shown {
        match levels {
            Levels::Flat => Shown::everything(),
            Levels::DatabaseIsSchema
            | Levels::DatabaseThenSchema
            | Levels::DatabaseHoldsObjects => {
                let entries = self
                    .databases
                    .iter()
                    .filter(|entry| !entry.trim().is_empty())
                    .cloned()
                    .collect::<Vec<_>>();
                Shown {
                    databases: (!entries.is_empty()).then_some(entries),
                    own_database: own_database.map(str::to_string),
                }
            }
        }
    }
}

/// The lists resolved for one target, owned so it moves into the closure that
/// reads on a background thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    /// The entries a database must match, or none for every database.
    databases: Option<Vec<String>>,
    /// The database the session is on, which is shown whatever matches.
    own_database: Option<String>,
}

impl Shown {
    /// No narrowing: what a read that wants the whole server asks for, and
    /// what a target with nothing to filter resolves to.
    pub fn everything() -> Self {
        Self::default()
    }

    pub fn shows_database(&self, name: &str) -> bool {
        let Some(entries) = &self.databases else {
            return true;
        };
        self.own_database
            .as_deref()
            .is_some_and(|own| same_name(own, name))
            || entries.iter().any(|entry| matches(entry, name))
    }

    /// `databases` with the names this does not show taken out, in the order
    /// the server listed them, and how many that was recorded in `left_out`.
    /// `current` is the session's own and never narrowed.
    pub fn narrow_databases(&self, mut databases: Databases) -> Databases {
        let listed = databases.names.len();
        databases.names.retain(|name| self.shows_database(name));
        databases.left_out = listed - databases.names.len();
        databases
    }
}

fn lowered(text: &str) -> impl Iterator<Item = char> {
    text.chars().flat_map(char::to_lowercase)
}

fn same_name(a: &str, b: &str) -> bool {
    lowered(a).eq(lowered(b))
}

/// Whether `name` is `pattern`: exact, or with `*` for any run of characters
/// (none included) and `?` for any one. Case is ignored by Unicode simple
/// lowercase of each character, and nothing else is normalized: a name is
/// compared as the server lists it. `_` and `%` are plain characters. An empty
/// pattern matches nothing.
///
/// Iterative, remembering only the last `*` and what it has eaten, so a
/// hand-edited file cannot make it exponential.
///
/// ponytail: a database whose real name holds `*` or `?` reads as a pattern.
/// The ceiling is that such a name cannot be ticked alone; the upgrade is an
/// escape.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let pattern: Vec<char> = lowered(pattern).collect();
    let name: Vec<char> = lowered(name).collect();
    let (mut at, mut on) = (0, 0);
    // Where the last `*` was and how much of the name it has eaten.
    let mut resume: Option<(usize, usize)> = None;
    while on < name.len() {
        match pattern.get(at) {
            Some('*') => {
                resume = Some((at, on));
                at += 1;
            }
            Some('?') => {
                at += 1;
                on += 1;
            }
            Some(&literal) if literal == name[on] => {
                at += 1;
                on += 1;
            }
            _ => match resume {
                Some((star, eaten)) => {
                    at = star + 1;
                    on = eaten + 1;
                    resume = Some((star, eaten + 1));
                }
                None => return false,
            },
        }
    }
    pattern[at..].iter().all(|&rest| rest == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(names: &[&str], current: Option<&str>) -> Databases {
        Databases {
            names: names.iter().map(|name| (*name).to_string()).collect(),
            current: current.map(str::to_string),
            left_out: 0,
        }
    }

    fn text(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    const FILTERED: [Levels; 3] = [
        Levels::DatabaseIsSchema,
        Levels::DatabaseThenSchema,
        Levels::DatabaseHoldsObjects,
    ];

    #[test]
    fn a_star_matches_any_run_and_a_question_mark_one_character() {
        for name in ["a", "ab", "", "dbdelve_dev"] {
            assert!(matches("*", name), "{name:?}");
        }
        assert!(matches("a*b", "ab"));
        assert!(matches("a*b", "axyb"));
        assert!(!matches("a*b", "abx"));
        assert!(matches("db?", "db1"));
        assert!(!matches("db?", "db"));
        assert!(!matches("db?", "db12"));
        assert!(matches("?", "x"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing_and_underscore_and_percent_are_literal() {
        assert!(!matches("", "a"));
        assert!(!matches("", ""));
        assert!(matches("a_b", "a_b"));
        assert!(!matches("a_b", "axb"));
        assert!(matches("a%", "a%"));
        assert!(!matches("a%", "ab"));
    }

    #[test]
    fn matching_ignores_case_by_unicode_lowercase() {
        assert!(matches("PROD*", "prod_eu"));
        assert!(matches("prod_*", "PROD_EU"));
        assert!(matches("Ä*", "ä_db"));
        assert!(matches("snowflake_db", "SNOWFLAKE_DB"));
    }

    #[test]
    fn whether_a_name_is_shown_does_not_depend_on_entry_order() {
        let entries = ["a*", "b", "?c"];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let answers = |order: [usize; 3]| {
            let include = Include::from_stored(
                order.iter().map(|at| entries[*at].to_string()).collect(),
                Vec::new(),
            );
            let shown = include.resolve(Levels::DatabaseThenSchema, None);
            ["ab", "b", "xc", "zz"].map(|name| shown.shows_database(name))
        };
        let first = answers(orders[0]);
        assert_eq!(first, [true, true, true, false]);
        for order in orders {
            assert_eq!(answers(order), first, "{order:?}");
        }
    }

    #[test]
    fn a_pathological_pattern_answers_without_blowing_up() {
        assert!(!matches("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(10_000)));
    }

    #[test]
    fn an_absent_empty_or_blank_list_shows_everything() {
        let names = ["a", "b", "mysql"];
        for include in [
            Include::from_stored(Vec::new(), Vec::new()),
            Include::from_stored(text(&["", "  "]), Vec::new()),
            Include::default(),
        ] {
            for levels in FILTERED {
                let narrowed = include
                    .resolve(levels, None)
                    .narrow_databases(listed(&names, None));
                assert_eq!(narrowed.names, names, "{levels:?}");
                assert_eq!(narrowed.left_out, 0, "{levels:?}");
            }
        }
    }

    #[test]
    fn a_stored_list_drops_blank_entries_and_keeps_at_most_the_cap() {
        let kept = Include::from_stored(text(&["b", "", "a*", "   ", "c"]), Vec::new());
        assert_eq!(kept.databases, ["b", "a*", "c"]);

        let many = (0..ENTRIES_CAP + 7)
            .map(|at| format!("db{at}"))
            .collect::<Vec<_>>();
        let capped = Include::from_stored(many.clone(), many.clone());
        assert_eq!(capped.databases.len(), ENTRIES_CAP);
        assert_eq!(capped.schemas.len(), ENTRIES_CAP);
        assert_eq!(capped.databases, many[..ENTRIES_CAP]);
    }

    #[test]
    fn each_level_filters_the_list_it_owns() {
        let include = Include::from_stored(text(&["a*"]), text(&["s"]));
        for levels in FILTERED {
            let narrowed = include
                .resolve(levels, None)
                .narrow_databases(listed(&["a1", "b1"], None));
            assert_eq!(narrowed.names, ["a1"], "{levels:?}");
        }
        assert_eq!(
            include.resolve(Levels::Flat, None),
            Shown::everything(),
            "nothing to choose between, so nothing is filtered"
        );

        let schemas_only = Include::from_stored(Vec::new(), text(&["s"]));
        for levels in FILTERED {
            let narrowed = schemas_only
                .resolve(levels, None)
                .narrow_databases(listed(&["a1", "b1"], None));
            assert_eq!(narrowed.names, ["a1", "b1"], "{levels:?}");
        }
    }

    #[test]
    fn the_connections_own_database_is_always_shown_and_counted_as_shown() {
        let include = Include::from_stored(text(&["nothing*"]), Vec::new());
        let narrowed = include
            .resolve(Levels::DatabaseThenSchema, Some("Own"))
            .narrow_databases(listed(&["a", "own", "b"], Some("own")));
        assert_eq!(narrowed.names, ["own"]);
        assert_eq!(narrowed.left_out, 2);
        assert_eq!(narrowed.current.as_deref(), Some("own"));

        let narrowed = include
            .resolve(Levels::DatabaseThenSchema, None)
            .narrow_databases(listed(&["a", "own", "b"], None));
        assert!(narrowed.names.is_empty());
        assert_eq!(narrowed.left_out, 3);
    }
}
