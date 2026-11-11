//! The filter picker: a dialog that edits one profile's include list.
//!
//! On open it reads the server's databases, and the own database's schemas
//! where the target has both levels, unfiltered whatever the tree shows, so a
//! name the filter hides can be ticked again. Save writes the profile's lists
//! and reads the catalog again; it never reconnects, because the lists are not
//! part of the connection. Which sections exist comes from `Target::levels()`,
//! never from the engine.

use gpui::SharedString;
use gpui_component::checkbox::Checkbox;

use super::*;
use crate::db::{Include, Levels, Shown};
use crate::ui::well;

/// Which of the two lists a section edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    Databases,
    Schemas,
}

impl Which {
    fn label(self) -> &'static str {
        match self {
            Self::Databases => "Databases",
            Self::Schemas => "Schemas",
        }
    }

    /// Singular only when the list holds one name.
    fn noun(self, total: usize) -> &'static str {
        match (self, total == 1) {
            (Self::Databases, true) => "database",
            (Self::Databases, false) => "databases",
            (Self::Schemas, true) => "schema",
            (Self::Schemas, false) => "schemas",
        }
    }

    /// What the dialog's ids and selectors call it.
    fn key(self) -> &'static str {
        match self {
            Self::Databases => "databases",
            Self::Schemas => "schemas",
        }
    }
}

/// Whether a target of shape `levels` has schemas to choose between inside a
/// database. Where a database is its own schema, or holds objects directly,
/// the one list is the whole filter.
fn lists_schemas(levels: Levels) -> bool {
    levels == Levels::DatabaseThenSchema
}

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
    /// The schema the connection is in, shown whatever the schemas list says
    /// in the own database.
    active_schema: Option<String>,
    /// What the profile held when the dialog opened.
    saved: Include,
    pub(crate) databases: PickerSection,
    /// The own database's schemas, on a target that has both levels.
    pub(crate) schemas: Option<PickerSection>,
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

    /// The same for the active schema, which only the own database's list
    /// holds: the schema list the dialog fetches is that one's.
    pub(crate) fn schema_hint(&self, name: &str) -> Option<&'static str> {
        self.active_schema
            .as_deref()
            .is_some_and(|active| same_name(active, name))
            .then_some("active schema")
    }

    fn hint(&self, which: Which, name: &str) -> Option<&'static str> {
        match which {
            Which::Databases => self.row_hint(name),
            Which::Schemas => self.schema_hint(name),
        }
    }

    fn section(&self, which: Which) -> Option<&PickerSection> {
        match which {
            Which::Databases => Some(&self.databases),
            Which::Schemas => self.schemas.as_ref(),
        }
    }

    fn section_mut(&mut self, which: Which) -> Option<&mut PickerSection> {
        match which {
            Which::Databases => Some(&mut self.databases),
            Which::Schemas => self.schemas.as_mut(),
        }
    }

    pub(crate) fn heading(&self) -> &'static str {
        match self.schemas {
            Some(_) => "Filter databases and schemas",
            None => "Filter databases",
        }
    }

    /// Says which database the schema list is of, and that the filter reaches
    /// the others too.
    pub(crate) fn schemas_caption(&self) -> Option<String> {
        self.schemas.as_ref()?;
        let database = self.own_database.as_deref()?;
        Some(format!(
            "Schemas of {database}. The schema filter applies inside every database."
        ))
    }

    fn loading(&self) -> bool {
        [Which::Databases, Which::Schemas]
            .into_iter()
            .filter_map(|which| self.section(which))
            .any(|section| section.listing == Listing::Loading)
    }

    /// What Save writes: each section's entries, and for a list with no
    /// section the one the profile held.
    fn written(&self) -> Include {
        Include {
            databases: self.databases.entries(),
            schemas: self
                .schemas
                .as_ref()
                .map_or_else(|| self.saved.schemas.clone(), PickerSection::entries),
        }
    }

    /// Nothing to write while a section is still being read, or when the lists
    /// are the ones the profile already holds.
    pub(crate) fn can_save(&self) -> bool {
        !self.loading() && self.written() != self.saved
    }

    /// The line under a list: how many of the listed names the list, as
    /// ticked and typed, shows now.
    pub(crate) fn preview(&self, which: Which) -> Option<String> {
        let section = self.section(which)?;
        let names = match &section.listing {
            Listing::Loading => return None,
            Listing::Failed(_) => {
                return Some("Matches cannot be previewed: the names were not read.".into());
            }
            Listing::Loaded(names) => names,
        };
        let total = names.len();
        let noun = which.noun(total);
        let place = match which {
            Which::Databases => String::new(),
            Which::Schemas => self
                .own_database
                .as_deref()
                .map(|database| format!(" in {database}"))
                .unwrap_or_default(),
        };
        let entries = section.entries();
        if entries.is_empty() {
            return Some(format!("Showing all {total} {noun}{place}"));
        }
        let include = match which {
            Which::Databases => Include {
                databases: entries,
                schemas: Vec::new(),
            },
            Which::Schemas => Include {
                databases: Vec::new(),
                schemas: entries,
            },
        };
        let shown = include.resolve(
            self.levels,
            self.own_database.as_deref(),
            self.active_schema.as_deref(),
        );
        let count = names
            .iter()
            .filter(|name| match which {
                Which::Databases => shown.shows_database(name),
                Which::Schemas => shown.shows_schema(name),
            })
            .count();
        Some(format!("Showing {count} of {total} {noun}{place}"))
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
    /// databases to choose between, and read the server's databases for it,
    /// and the own database's schemas where there are two levels.
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
        let wants_schemas = lists_schemas(levels);
        let in_transaction = found.in_transaction;
        self.last_filter_request += 1;
        let request = self.last_filter_request;
        self.filter_picker = Some(FilterPicker {
            profile: found.id.clone(),
            request,
            levels,
            own_database: found.active_database(),
            active_schema: found.active_schema().map(str::to_string),
            saved: found.include.clone(),
            databases: PickerSection::loading(&found.include.databases),
            schemas: wants_schemas.then(|| PickerSection::loading(&found.include.schemas)),
        });
        cx.notify();
        // Every database and schema, not the shown ones: the names the tree
        // holds are only what the filter lets through.
        let fetch = spawn_blocking(cx.background_executor(), move || {
            let databases = connection.databases(target.engine, &Shown::everything());
            let schemas = wants_schemas.then(|| {
                // A failed read inside the user's transaction fails the
                // transaction on Postgres, so it is read on another session.
                let reader = match in_transaction {
                    true => connection.apart()?,
                    false => connection.clone(),
                };
                let catalog = reader.catalog(&Shown::everything())?;
                Ok::<_, DbError>(
                    catalog
                        .schemas
                        .into_iter()
                        .map(|schema| schema.name)
                        .collect::<Vec<_>>(),
                )
            });
            (databases, schemas)
        });
        cx.spawn(async move |workspace, cx| {
            let (databases, schemas) = fetch.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.names_listed_for_picker(request, databases, schemas, cx);
            });
        })
        .detach();
    }

    fn names_listed_for_picker(
        &mut self,
        request: u64,
        databases: Result<Databases, DbError>,
        schemas: Option<Result<Vec<String>, DbError>>,
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
        match databases {
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
        if let (Some(section), Some(result)) = (picker.schemas.as_mut(), schemas) {
            match result {
                Ok(names) => {
                    let (ticked, patterns) = split(&picker.saved.schemas, &names);
                    section.ticked = ticked;
                    section.patterns = patterns;
                    section.listing = Listing::Loaded(names);
                }
                Err(error) => section.listing = Listing::Failed(error.message),
            }
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

    fn tick_name(&mut self, which: Which, name: &str, on: bool, cx: &mut Context<Self>) {
        let Some(section) = self
            .filter_picker
            .as_mut()
            .and_then(|picker| picker.section_mut(which))
        else {
            return;
        };
        section.ticked.retain(|ticked| ticked != name);
        if on {
            section.ticked.push(name.to_string());
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
        let include = picker.written();
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

    /// One list of the dialog: its label, what it is of, the names over a
    /// well, and how many of them Save will show.
    fn render_section(
        &self,
        picker: &FilterPicker,
        which: Which,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let section = picker.section(which)?;
        let t = popup_theme(cx);
        let key = which.key();
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let row_height = px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT));
        let line = |selector: String, ink: crate::theme::color::Srgb, text: String| {
            div()
                .debug_selector(move || selector.clone())
                .px(px(layout::SPACE_SM))
                .py(px(layout::SPACE_XS))
                .text_size(text_size)
                .text_color(ink)
                .child(text)
                .into_any_element()
        };
        let list = match &section.listing {
            Listing::Loading => line(
                format!("filter-picker-loading-{key}"),
                t.text_muted,
                format!("Listing {key}…"),
            ),
            Listing::Failed(message) => line(
                format!("filter-picker-failed-{key}"),
                t.danger,
                format!("The list of {key} could not be read: {message}"),
            ),
            Listing::Loaded(names) if names.is_empty() => line(
                format!("filter-picker-empty-{key}"),
                t.text_muted,
                match which {
                    Which::Databases => "The server lists no databases.".into(),
                    Which::Schemas => match picker.own_database.as_deref() {
                        Some(database) => format!("No schemas in {database}."),
                        None => "No schemas.".into(),
                    },
                },
            ),
            // ponytail: every row is drawn, not virtualised, so a server
            // listing thousands of names draws thousands of checkboxes.
            // The upgrade is `uniform_list`.
            Listing::Loaded(names) => div()
                .id(SharedString::from(format!("filter-picker-list-{key}")))
                .debug_selector(move || format!("filter-picker-list-{key}"))
                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(names.iter().enumerate().map(|(index, listed)| {
                    let hint = picker.hint(which, listed);
                    let checked = hint.is_some() || section.ticked.contains(listed);
                    let toggled = listed.clone();
                    div()
                        .h(row_height)
                        .px(px(layout::SPACE_SM))
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_XS))
                        .child(
                            div()
                                .debug_selector(move || format!("filter-picker-row-{key}-{index}"))
                                .flex_1()
                                .min_w_0()
                                .child(
                                    Checkbox::new(SharedString::from(format!(
                                        "filter-picker-{key}-{index}"
                                    )))
                                    .w_full()
                                    .text_size(text_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .accessibility_label(listed.clone())
                                    .checked(checked)
                                    .disabled(hint.is_some())
                                    .on_click(cx.listener(move |workspace, on: &bool, _, cx| {
                                        workspace.tick_name(which, &toggled, *on, cx);
                                    }))
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
        let caption = match which {
            Which::Databases => None,
            Which::Schemas => picker.schemas_caption(),
        };
        Some(
            div()
                .flex_1()
                .min_w(px(layout::SETTINGS_TEXT_MIN_WIDTH))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(section_label(t, which.label()))
                .children(caption.map(|caption| {
                    div()
                        .text_size(text_size)
                        .text_color(t.text_muted)
                        .child(caption)
                }))
                .child(well(t, [list]))
                .children(picker.preview(which).map(|preview| {
                    div()
                        .debug_selector(move || format!("filter-picker-preview-{key}"))
                        .text_size(text_size)
                        .text_color(t.text_muted)
                        .child(preview)
                }))
                .into_any_element(),
        )
    }

    /// The dialog: a checklist per list over what Save will show of it.
    pub(crate) fn render_filter_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.filter_picker_in_front()?;
        let name = self.profile()?.name.clone();
        let t = popup_theme(cx);
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let sections: Vec<AnyElement> = [Which::Databases, Which::Schemas]
            .into_iter()
            .filter_map(|which| self.render_section(picker, which, cx))
            .collect();
        let card = dialog(t).debug_selector(|| "filter-picker".into());
        // Two lists need two columns.
        let card = match picker.schemas {
            Some(_) => card
                .w(px(layout::APPROVAL_DIALOG_WIDTH))
                .max_w(gpui::relative(0.9)),
            None => card,
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
                    card.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .child(section_label(t, picker.heading()))
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
                            .flex_wrap()
                            .gap(px(layout::SPACE_MD))
                            .children(sections),
                    )
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
