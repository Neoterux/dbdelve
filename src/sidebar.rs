//! What the connections sidebar lists, as plain data.
//!
//! Complex mode draws every connection under its project, and each
//! connection's own rows are the ones simple mode draws for it. Two rules hold
//! the model together. Ids are length-prefixed, so no name -- a profile's, a
//! project's, a table's -- can run on into the next segment and be read as
//! another row's. And [`build`] takes loaded data only: it neither connects,
//! reads a catalog nor sees a window, so what a row says is what the caller
//! had already been told.

use std::{collections::HashMap, ops::Range};

use gpui_component::tree::TreeItem;

use crate::explorer::{ExplorerLeaf, ExplorerTree, Shortcut};
use crate::store::StoredProject;

/// Which sidebar the window draws: today's tree for the connection in front,
/// or every connection under its project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ViewMode {
    #[default]
    Simple,
    Complex,
}

impl ViewMode {
    /// How `profiles.toml` spells it.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            ViewMode::Simple => "simple",
            ViewMode::Complex => "complex",
        }
    }

    /// The mode a stored slug names. A file with none, or one this build does
    /// not know, reads as Simple: what every file written before it showed.
    pub(crate) fn from_slug(slug: Option<&str>) -> Self {
        match slug {
            Some("complex") => ViewMode::Complex,
            _ => ViewMode::Simple,
        }
    }
}

/// The row of connections that belong to no project, listed first while any
/// project exists.
pub(crate) const NO_PROJECT_ROW: &str = "no-project";

/// A project's row id. The name follows its length, as a folder's does.
pub(crate) fn project_row(name: &str) -> String {
    format!("project:{}:{name}", name.len())
}

/// A connection's row id, by the profile's id.
pub(crate) fn connection_row(profile: &str) -> String {
    format!("connection:{}:{profile}", profile.len())
}

/// The id of `row`, one of simple mode's rows, listed under `profile`. The
/// profile's length keeps a `/` in either from moving the seam.
pub(crate) fn scoped_row(profile: &str, row: &str) -> String {
    format!("{}/{row}", connection_row(profile))
}

/// The profile and the simple-mode row id a [`scoped_row`] was made from.
/// `None` for any other id, including a connection's own row.
pub(crate) fn unscoped(id: &str) -> Option<(&str, &str)> {
    let (length, rest) = id.strip_prefix("connection:")?.split_once(':')?;
    let profile = rest.get(..length.parse().ok()?)?;
    let row = rest.get(profile.len()..)?.strip_prefix('/')?;
    // Read back the way it was written: `+3` parses as a length, and would
    // otherwise name the same row as `3`.
    (scoped_row(profile, row) == id).then_some((profile, row))
}

/// What a connection that has nothing to list says in its one row.
pub(crate) const LOADING: &str = "Loading…";

/// How much of a connection is known.
pub(crate) enum Content {
    /// Never asked to load: it connects when first expanded.
    Unloaded,
    Loading,
    Failed(String),
    Loaded(ExplorerTree),
}

/// One connection, as the caller has it.
pub(crate) struct Listed<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) content: Content,
}

/// What a row stands for.
pub(crate) enum Node {
    /// A project, or the connections in none.
    Group,
    Connection {
        profile: String,
    },
    /// Says what happened to its connection, and is never acted on.
    Notice,
    /// One of simple mode's rows, its leaf when it opens an object.
    Row {
        leaf: Option<ExplorerLeaf>,
    },
}

/// The rows, and what each stands for, by row id.
#[derive(Default)]
pub(crate) struct SidebarTree {
    pub(crate) items: Vec<TreeItem>,
    pub(crate) nodes: HashMap<String, Node>,
    pub(crate) highlights: HashMap<String, Vec<Range<usize>>>,
    pub(crate) shortcuts: HashMap<String, Shortcut>,
}

/// The rows for `listed`, grouped under `projects` the way the palette's
/// Connections scope orders them: the connections in no project first, and
/// only while a project exists, then each project in its stored order, every
/// connection in the order `listed` has it. Nothing is sorted, so two builds
/// over the same input give the same rows.
///
/// `folds` is whether each row was open in the tree this one replaces, by
/// row id, so a rebuild puts the user's folding back.
pub(crate) fn build(
    projects: &[StoredProject],
    listed: Vec<Listed<'_>>,
    folds: &HashMap<String, bool>,
) -> SidebarTree {
    let mut tree = SidebarTree::default();
    // One bucket per project, and the first for the connections in none. A
    // connection a second project also names stays under the first, as it
    // does in the palette.
    let mut buckets: Vec<Vec<TreeItem>> = vec![Vec::new(); projects.len() + 1];
    for connection in listed {
        let bucket = projects
            .iter()
            .position(|project| project.connections.iter().any(|id| id == connection.id))
            .map_or(0, |at| at + 1);
        buckets[bucket].push(connection_item(&mut tree, connection, folds));
    }
    if projects.is_empty() {
        tree.items = buckets.swap_remove(0);
        return tree;
    }
    let mut buckets = buckets.into_iter();
    // A group with nothing in it is a row that only says so, which is wanted
    // of a project the user made and not of the absence of one.
    let unlisted = buckets.next().unwrap_or_default();
    let groups = (!unlisted.is_empty())
        .then(|| (NO_PROJECT_ROW.to_string(), "No project", unlisted))
        .into_iter()
        .chain(projects.iter().zip(buckets).map(|(project, members)| {
            (project_row(&project.name), project.name.as_str(), members)
        }));
    for (id, label, members) in groups {
        tree.nodes.insert(id.clone(), Node::Group);
        let open = folds.get(&id).copied().unwrap_or(true);
        tree.items.push(
            TreeItem::new(id, label.to_string())
                .expanded(open)
                .children(members),
        );
    }
    tree
}

fn connection_item(
    tree: &mut SidebarTree,
    connection: Listed<'_>,
    folds: &HashMap<String, bool>,
) -> TreeItem {
    let id = connection_row(connection.id);
    tree.nodes.insert(
        id.clone(),
        Node::Connection {
            profile: connection.id.to_string(),
        },
    );
    let followed = folds.get(&id).copied().unwrap_or(false);
    let (open, children) = match connection.content {
        // Nothing has asked it to load, and nothing will until this row is
        // opened: an open row here would show a `Loading…` that no answer is
        // coming to replace.
        Content::Unloaded => (false, vec![notice(tree, connection.id, "loading", LOADING)]),
        Content::Loading => (
            followed,
            vec![notice(tree, connection.id, "loading", LOADING)],
        ),
        Content::Failed(text) => (followed, vec![notice(tree, connection.id, "notice", &text)]),
        Content::Loaded(content) if content.items.is_empty() => (
            followed,
            vec![notice(
                tree,
                connection.id,
                "notice",
                "No database objects found.",
            )],
        ),
        Content::Loaded(content) => {
            let ExplorerTree {
                items,
                leaves,
                highlights,
                shortcuts,
            } = content;
            let children = items
                .into_iter()
                .map(|item| scoped_item(tree, connection.id, item, &leaves, folds))
                .collect();
            for (row, ranges) in highlights {
                tree.highlights
                    .insert(scoped_row(connection.id, &row), ranges);
            }
            for (row, shortcut) in shortcuts {
                tree.shortcuts
                    .insert(scoped_row(connection.id, &row), shortcut);
            }
            (followed, children)
        }
    };
    TreeItem::new(id, connection.name.to_string())
        .expanded(open)
        .children(children)
}

/// A row that says something about its connection and opens nothing. Its id
/// is one no simple-mode row can have.
fn notice(tree: &mut SidebarTree, profile: &str, row: &str, text: &str) -> TreeItem {
    let id = scoped_row(profile, row);
    tree.nodes.insert(id.clone(), Node::Notice);
    TreeItem::new(id, text.to_string()).disabled(true)
}

/// `item` and everything under it with its id scoped to `profile`. A folder
/// is folded unless `folds` says it was open: a connect must not unfold the
/// thousands of rows a catalog can hold.
fn scoped_item(
    tree: &mut SidebarTree,
    profile: &str,
    mut item: TreeItem,
    leaves: &HashMap<String, ExplorerLeaf>,
    folds: &HashMap<String, bool>,
) -> TreeItem {
    let id = scoped_row(profile, &item.id);
    tree.nodes.insert(
        id.clone(),
        Node::Row {
            leaf: leaves.get(item.id.as_str()).copied(),
        },
    );
    let folder = item.is_folder();
    let disabled = item.is_disabled();
    let label = item.label.clone();
    let children: Vec<TreeItem> = std::mem::take(&mut item.children)
        .into_iter()
        .map(|child| scoped_item(tree, profile, child, leaves, folds))
        .collect();
    let open = folder && folds.get(&id).copied().unwrap_or(false);
    TreeItem::new(id, label)
        .expanded(open)
        .disabled(disabled)
        .children(children)
}
