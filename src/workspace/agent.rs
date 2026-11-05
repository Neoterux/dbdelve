//! The one place a profile's agent visibility changes.
//!
//! One door, because a sweep of the agent calls still pending against a
//! profile (Phase 4) has to run exactly when the profile is hidden, and a
//! second writer would be one more path to tell about it. Granting is the
//! asymmetric half: it needs the owner's confirmation, so it is refused unless
//! the visibility dialog is open for that very profile.

use super::*;

impl Workspace {
    /// Make a profile visible to agents or hide it. Keyed by id, never by
    /// index: ids can repeat in a hand-edited file, and the first one wins as
    /// everywhere else in `profiles.rs`.
    ///
    /// Asked for `true` this changes nothing unless
    /// [`Workspace::agent_visibility_prompt`] holds the profile's id, and takes
    /// the prompt when it grants. Asked for `false` it hides at once, because
    /// narrowing needs no confirmation.
    pub(crate) fn set_agent_visible(&mut self, id: &str, visible: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        if visible {
            if self.agent_visibility_prompt.as_deref() != Some(id) {
                return;
            }
            self.agent_visibility_prompt = None;
        }
        profile.agent_visible = visible;
        self.remember_profiles(cx);
        cx.notify();
    }
}
