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

pub(crate) fn build(
    _projects: &[StoredProject],
    _listed: Vec<Listed<'_>>,
    _folds: &HashMap<String, bool>,
) -> SidebarTree {
    SidebarTree::default()
}
