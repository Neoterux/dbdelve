//! The filter picker: a dialog that edits one profile's include list.
//!
//! On open it reads the server's databases unfiltered, whatever the tree
//! shows, so a name the filter hides can be ticked again. Save writes the
//! profile's list and reads the catalog again; it never reconnects, because
//! the list is not part of the connection. Which sections exist comes from
//! `Target::levels()`, never from the engine.

use gpui_component::checkbox::Checkbox;

use super::*;
use crate::db::{Include, Levels, Shown};
use crate::ui::well;

/// What a section's full list of names has got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Listing {
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

/// One list of the profile's, as the dialog edits it.
pub(crate) struct PickerSection {
    pub(crate) listing: Listing,
    /// The listed names ticked by hand, in the spelling the server lists.
    pub(crate) ticked: Vec<String>,
    /// Every saved entry that is not a ticked name: wildcards, and names the
    /// server no longer lists. Written back by Save, so an entry the dialog
    /// has no field for yet is never dropped.
    pub(crate) patterns: Vec<String>,
}

pub(crate) struct FilterPicker {
    /// The connection being edited, by id.
    pub(crate) profile: String,
    /// Numbers each open, so a listing that lands after the dialog closed, or
    /// after another was put up, changes nothing.
    request: u64,
    levels: Levels,
    /// The database the connection is in, which is shown whatever the list says.
    own_database: Option<String>,
    /// What the profile held when the dialog opened.
    saved: Include,
    pub(crate) databases: PickerSection,
}

impl PickerSection {
    /// Before the names arrive every saved entry waits as a pattern, so a
    /// list that never loads still writes back what it held.
    fn loading(saved: &[String]) -> Self {
        Self {
            listing: Listing::Loading,
            ticked: Vec::new(),
            patterns: saved.to_vec(),
        }
    }

    /// The list Save writes: ticked names in the order and spelling the server
    /// lists them, then the kept patterns, blanks dropped and duplicates
    /// ignoring case dropped.
    fn entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = match &self.listing {
            Listing::Loaded(names) => names
                .iter()
                .filter(|name| self.ticked.contains(name))
                .cloned()
                .collect(),
            _ => self.ticked.clone(),
        };
        for pattern in &self.patterns {
            let repeated = entries.iter().any(|entry| same_name(entry, pattern));
            if !pattern.trim().is_empty() && !repeated {
                entries.push(pattern.clone());
            }
        }
        entries
    }
}

impl FilterPicker {
    /// The always-shown rows say why they are: the connection's own database
    /// is checked and cannot be unticked.
    pub(crate) fn row_hint(&self, name: &str) -> Option<&'static str> {
        self.own_database
            .as_deref()
            .is_some_and(|own| same_name(own, name))
            .then_some("current database")
    }

    fn loading(&self) -> bool {
        self.databases.listing == Listing::Loading
    }

    /// Nothing to write while a section is still being read, or when the lists
    /// are the ones the profile already holds.
    fn can_save(&self) -> bool {
        !self.loading() && self.databases.entries() != self.saved.databases
    }

    /// The line under the list: how many of the listed names the lists, as
    /// ticked and typed, show now.
    fn preview(&self) -> Option<String> {
        let names = match &self.databases.listing {
            Listing::Loading => return None,
            Listing::Failed(_) => {
                return Some("Matches cannot be previewed: the names were not read.".into());
            }
            Listing::Loaded(names) => names,
        };
        let total = names.len();
        let noun = if total == 1 { "database" } else { "databases" };
        let entries = self.databases.entries();
        if entries.is_empty() {
            return Some(format!("Showing all {total} {noun}"));
        }
        let shown = Include {
            databases: entries,
            schemas: Vec::new(),
        }
        .resolve(self.levels, self.own_database.as_deref(), None);
        let count = names
            .iter()
            .filter(|name| shown.shows_database(name))
            .count();
        Some(format!("Showing {count} of {total} {noun}"))
    }
}

/// Whether two names are one, ignoring case as the filter does.
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The saved entries as rows: an entry that names a listed name, with no
/// wildcard, is that row ticked; every other entry is kept as typed.
fn split(saved: &[String], names: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut ticked, mut patterns) = (Vec::new(), Vec::new());
    for entry in saved {
        let listed = (!entry.contains(['*', '?']))
            .then(|| names.iter().find(|name| same_name(name, entry)))
            .flatten();
        match listed {
            Some(name) if ticked.contains(name) => {}
            Some(name) => ticked.push(name.clone()),
            None => patterns.push(entry.clone()),
        }
    }
    (ticked, patterns)
}

impl Workspace {
    /// Put the picker up over `profile`, which must be connected and have
    /// databases to choose between, and read the server's databases for it.
    pub(crate) fn open_filter_picker(
        &mut self,
        profile: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(found) = self.profiles.iter().find(|found| found.id == profile) else {
            return;
        };
        let target = found.config.target();
        let levels = target.levels();
        if levels == Levels::Flat {
            return;
        }
        let Some(connection) = found.connection() else {
            return;
        };
        self.last_filter_request += 1;
        let request = self.last_filter_request;
        self.filter_picker = Some(FilterPicker {
            profile: found.id.clone(),
            request,
            levels,
            own_database: found.active_database(),
            saved: found.include.clone(),
            databases: PickerSection::loading(&found.include.databases),
        });
        cx.notify();
        // Every database, not the shown ones: the names the tree holds are
        // only what the filter lets through.
        let fetch = spawn_blocking(cx.background_executor(), move || {
            connection.databases(target.engine, &Shown::everything())
        });
        cx.spawn(async move |workspace, cx| {
            let result = fetch.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.databases_listed_for_picker(request, result, cx);
            });
        })
        .detach();
    }

    fn databases_listed_for_picker(
        &mut self,
        request: u64,
        result: Result<Databases, DbError>,
        cx: &mut Context<Self>,
    ) {
        // Closed, or put up again, since: the answer is not this dialog's.
        if self
            .filter_picker
            .as_ref()
            .is_none_or(|picker| picker.request != request)
        {
            return;
        }
        // Its connection went from the front while it was read, and nothing
        // draws the dialog to answer.
        if self.filter_picker_in_front().is_none() {
            self.filter_picker = None;
            return;
        }
        let Some(picker) = self.filter_picker.as_mut() else {
            return;
        };
        match result {
            Ok(listed) => {
                let (ticked, patterns) = split(&picker.saved.databases, &listed.names);
                picker.databases.ticked = ticked;
                picker.databases.patterns = patterns;
                picker.databases.listing = Listing::Loaded(listed.names);
                if listed.current.is_some() {
                    picker.own_database = listed.current;
                }
            }
            Err(error) => picker.databases.listing = Listing::Failed(error.message),
        }
        cx.notify();
    }

    /// The picker, when it is the front connection's: one left up for a
    /// connection that is no longer in front is not drawn and `escape` does
    /// not stop at it.
    pub(crate) fn filter_picker_in_front(&self) -> Option<&FilterPicker> {
        let profile = self.profile()?;
        self.filter_picker
            .as_ref()
            .filter(|picker| picker.profile == profile.id)
    }

    /// Close the dialog and hand the keyboard back. Answers whether it was
    /// drawn, so `escape` is only spent on a dialog that was.
    pub(crate) fn close_filter_picker(&mut self, cx: &mut Context<Self>) -> bool {
        if self.filter_picker_in_front().is_none() {
            return false;
        }
        self.filter_picker = None;
        self.refocus_front();
        cx.notify();
        true
    }

    fn tick_database(&mut self, name: &str, on: bool, cx: &mut Context<Self>) {
        let Some(picker) = self.filter_picker.as_mut() else {
            return;
        };
        picker.databases.ticked.retain(|ticked| ticked != name);
        if on {
            picker.databases.ticked.push(name.to_string());
        }
        cx.notify();
    }

    /// Write the lists to the profile and read its catalog again. The lists
    /// are not part of the connection, so nothing reconnects, no tab closes
    /// and the agent keeps its view of the profile.
    pub(crate) fn save_filter(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self
            .filter_picker_in_front()
            .filter(|picker| picker.can_save())
        else {
            return;
        };
        let id = picker.profile.clone();
        let include = Include {
            databases: picker.databases.entries(),
            // Not edited here: written back as the profile held it.
            schemas: picker.saved.schemas.clone(),
        };
        let Some(profile) = self.profiles.iter_mut().find(|found| found.id == id) else {
            return;
        };
        profile.include = include;
        // What the filter now hides is not read: its catalog goes. A session
        // a tab is bound to stays, since the tab keeps running on it.
        let shown = profile.shown();
        profile
            .database_catalogs
            .retain(|database, _| shown.shows_database(database));
        self.remember_profiles(cx);
        self.close_filter_picker(cx);
        self.refresh_schema(cx);
    }

    /// The dialog: the databases checklist over what Save will show of them.
    pub(crate) fn render_filter_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.filter_picker_in_front()?;
        let name = self.profile()?.name.clone();
        let t = popup_theme(cx);
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let row_height = px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT));
        let line = |selector: &'static str, ink: crate::theme::color::Srgb, text: String| {
            div()
                .debug_selector(move || selector.to_string())
                .px(px(layout::SPACE_SM))
                .py(px(layout::SPACE_XS))
                .text_size(text_size)
                .text_color(ink)
                .child(text)
                .into_any_element()
        };
        let list = match &picker.databases.listing {
            Listing::Loading => line(
                "filter-picker-loading-databases",
                t.text_muted,
                "Listing databases…".into(),
            ),
            Listing::Failed(message) => line(
                "filter-picker-failed-databases",
                t.danger,
                format!("The list of databases could not be read: {message}"),
            ),
            Listing::Loaded(names) if names.is_empty() => line(
                "filter-picker-empty-databases",
                t.text_muted,
                "The server lists no databases.".into(),
            ),
            // ponytail: every row is drawn, not virtualised, so a server
            // listing thousands of databases draws thousands of checkboxes.
            // The upgrade is `uniform_list`.
            Listing::Loaded(names) => div()
                .id("filter-picker-list-databases")
                .debug_selector(|| "filter-picker-list-databases".into())
                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(names.iter().enumerate().map(|(index, listed)| {
                    let hint = picker.row_hint(listed);
                    let checked = hint.is_some() || picker.databases.ticked.contains(listed);
                    let toggled = listed.clone();
                    div()
                        .h(row_height)
                        .px(px(layout::SPACE_SM))
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_XS))
                        .child(
                            div()
                                .debug_selector(move || {
                                    format!("filter-picker-row-databases-{index}")
                                })
                                .flex_1()
                                .min_w_0()
                                .child(
                                    Checkbox::new(("filter-picker-databases", index))
                                        .w_full()
                                        .text_size(text_size)
                                        .font_weight(FontWeight::MEDIUM)
                                        .accessibility_label(listed.clone())
                                        .checked(checked)
                                        .disabled(hint.is_some())
                                        .on_click(cx.listener(
                                            move |workspace, on: &bool, _, cx| {
                                                workspace.tick_database(&toggled, *on, cx);
                                            },
                                        ))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(listed.clone()),
                                        ),
                                ),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                .text_color(t.text_muted)
                                .child(hint)
                        }))
                }))
                .into_any_element(),
        };
        let save = cx.entity().downgrade();
        let discard = save.clone();
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .debug_selector(|| "filter-picker".into())
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .child(section_label(t, "Filter databases"))
                                .child(
                                    div()
                                        .text_size(text_size)
                                        .text_color(t.text_muted)
                                        .child(name),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .child(section_label(t, "Databases"))
                                .child(well(t, [list])),
                        )
                        .children(picker.preview().map(|preview| {
                            div()
                                .debug_selector(|| "filter-picker-preview-databases".into())
                                .text_size(text_size)
                                .text_color(t.text_muted)
                                .child(preview)
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "filter-picker",
                                        1,
                                        button(
                                            "filter-picker-discard",
                                            "Discard Changes",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = discard.update(cx, |workspace, cx| {
                                                    workspace.close_filter_picker(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "filter-picker",
                                        0,
                                        button(
                                            "filter-picker-save",
                                            "Save Filter",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .disabled(!picker.can_save())
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = save.update(cx, |workspace, cx| {
                                                    workspace.save_filter(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_database_then_schema_target_lists_schemas() {
        assert!(lists_schemas(Levels::DatabaseThenSchema));
        assert!(!lists_schemas(Levels::DatabaseIsSchema));
        assert!(!lists_schemas(Levels::DatabaseHoldsObjects));
        assert!(!lists_schemas(Levels::Flat));
    }
}
