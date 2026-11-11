//! The connections sidebar of complex mode: every connection under its
//! project, loaded when first expanded.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::Entity;
use gpui_component::tree::{TreeEvent, TreeState};

use super::*;
use crate::explorer::Shortcut;
use crate::sidebar::Node;

pub(crate) struct ConnectionsSidebar {
    pub(crate) tree: Entity<TreeState>,
    pub(crate) nodes: Arc<HashMap<String, Node>>,
    pub(crate) highlights: Arc<HashMap<String, Vec<Range<usize>>>>,
    pub(crate) shortcuts: Arc<HashMap<String, Shortcut>>,
}

impl ConnectionsSidebar {
    pub(crate) fn new(cx: &mut Context<Workspace>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        cx.subscribe(&tree, |workspace, _, event: &TreeEvent, cx| {
            if let TreeEvent::Expanded(id) = event {
                workspace.expand_node(id, cx);
            }
        })
        .detach();
        Self {
            tree,
            nodes: Arc::default(),
            highlights: Arc::default(),
            shortcuts: Arc::default(),
        }
    }
}

impl Workspace {
    pub(crate) fn rebuild_connections(&mut self, _cx: &mut Context<Self>) {}

    pub(crate) fn expand_node(&mut self, _id: &gpui::SharedString, _cx: &mut Context<Self>) {}
}
