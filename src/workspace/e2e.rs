//! The app driven the way a person drives it: launched from a saved
//! `profiles.toml`, typed into, run from the keyboard. GPUI's test platform
//! has no screen, so these run headless in CI like any other test.

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, Window,
};
use gpui_component::{Root, input::InputState, tree::TreeEvent};

use super::Workspace;
#[cfg(unix)]
use crate::agent::tests::frame;
use crate::{
    actions::{CancelQuery, Disconnect, ImportRowsFromClipboard, SubmitConnection},
    connection_form::ConnectionForm,
    db::{CancelToken, ConnectionConfig, Engine, ExternalConfig, Fetch, RelationKind, SshTunnel},
    explain::analysis::Basis,
    explorer, keybindings,
    palette::{Command, Mode as PaletteMode, Scope},
    session::{CatalogState, ObjectBody, ProfileState, QueryState, QueryTab, Tab},
    sidebar::{
        LOADING, ListingScope, NO_PROJECT_ROW, ViewMode, connection_row, database_row, project_row,
        scoped_row,
    },
    store,
    theme::{self, Fonts},
};

/// What `main` sets up before the window opens, minus the menu bar and the
/// panic log, which the test platform has no use for.
fn launch(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_with(cx, Vec::new())
}

/// `launch` with these plugins discovered.
fn launch_with<'a>(
    cx: &'a mut TestAppContext,
    plugins: Vec<&'static crate::db::Plugin>,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        cx.text_system()
            .add_fonts(
                guic_gpui_assets::BUNDLED_FONTS
                    .iter()
                    .map(|font| Cow::Borrowed(*font))
                    .collect(),
            )
            .expect("bundled fonts must be loadable");
        gpui_component::init(cx);
        cx.set_global(Fonts::default());
        theme::user::load();
        let theme = theme::starting_theme(theme::Appearance::of_system(cx.window_appearance()));
        theme.apply_to_components(cx);
        cx.set_global(theme);
        let overrides = store::peek_profiles(&plugins)
            .and_then(|list| list.settings)
            .and_then(|settings| settings.custom_keybindings)
            .unwrap_or_default();
        cx.bind_keys(keybindings::build_bindings(&overrides));
    });
    let mut workspace = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(plugins, window, cx));
        workspace = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    let workspace = workspace.expect("the window builds the workspace");
    // The runner's own `PG*` variables reach `Workspace::new`: GitHub's Windows
    // image sets `PGUSER` and `PGPASSWORD` alone, which opens the connection
    // form over everything a test puts in front. Every test starts from
    // `profiles.toml` only.
    workspace.update(cx, |workspace, cx| {
        if workspace.form.is_some() {
            workspace.close_form(cx);
        }
    });
    settle(cx);
    (workspace, cx)
}

/// Every write `Workspace` makes on the way out -- buffers on release,
/// profiles on quit -- has to land while `with_home` still points the store at
/// the temporary home, not after the test returns and `HOME` is the real one.
fn quit(workspace: Entity<Workspace>, cx: &mut VisualTestContext) {
    drop(workspace);
    // On the app rather than the window: shutting down removes the window.
    cx.cx.update(|cx| cx.shutdown());
    cx.cx.run_until_parked();
}

/// The app launched on one saved SQLite profile, an empty database, with its
/// query tab in front.
fn launch_on_sqlite(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_on_sqlite_holding(cx, "")
}

/// `launch_on_sqlite` over a database `schema` has been run against first.
fn launch_on_sqlite_holding<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    // An empty file is an empty database. dbdelve opens a SQLite file
    // rather than creating one, so it has to be there first.
    let database = directory.join("e2e.db");
    std::fs::write(&database, b"").expect("the database file must be creatable");
    rusqlite::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(schema))
        .expect("the schema must apply");
    std::fs::write(
        directory.join("profiles.toml"),
        format!(
            "active = \"e2e\"\n\
             [settings]\n\
             check_for_updates = false\n\
             [[profiles]]\n\
             id = \"e2e\"\n\
             name = \"E2E\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"sqlite\"\n\
             path = {:?}\n",
            database.display().to_string()
        ),
    )
    .expect("profiles.toml must be writable");

    let (workspace, cx) = launch(cx);
    // CI's live-test job sets `PG*`, which puts an environment profile in
    // front of the saved one.
    workspace.update(cx, |workspace, cx| {
        let index = workspace
            .profiles
            .iter()
            .position(|profile| profile.config.engine() == Engine::Sqlite)
            .expect("the saved SQLite profile is restored");
        workspace.activate(index, cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

/// The app launched on `profiles`, the `[[profiles]]` tables of a hand-written
/// `profiles.toml`, with no profile chosen to be in front. `DB_PATH` in the text
/// stands for the path of an empty SQLite file written beside it, so a test can
/// say what a profile looks like on disk without knowing where the home is.
fn launch_with_profiles<'a>(
    cx: &'a mut TestAppContext,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_with_settings(cx, "", profiles)
}

/// `launch_with_profiles` with more `[settings]` keys, such as
/// `agent_access = true`.
fn launch_with_settings<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_connections(cx, "", settings, profiles)
}

/// `launch_with_settings` over the SQLite file `DB_PATH` names, `schema`
/// having been run against it first, as `launch_on_sqlite_holding` does for
/// its one profile: every profile that says `DB_PATH` lists the same tables.
fn launch_connections<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
    settings: &str,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.db");
    std::fs::write(&database, b"").expect("the database file must be creatable");
    rusqlite::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(schema))
        .expect("the schema must apply");
    // The first profile is put in front, as a launch that left it there does;
    // a file with no `active` starts on the welcome surface instead.
    let active = profiles
        .lines()
        .find_map(|line| line.strip_prefix("id = "))
        .map(|id| format!("active = {id}\n"))
        .unwrap_or_default();
    std::fs::write(
        directory.join("profiles.toml"),
        format!("{active}[settings]\ncheck_for_updates = false\n{settings}\n{profiles}")
            .replace("DB_PATH", &format!("{:?}", database.display().to_string())),
    )
    .expect("profiles.toml must be writable");
    launch(cx)
}

/// One SQLite profile as `profiles.toml` spells it. `extra` is more scalar
/// keys, which have to come before any table.
fn sqlite_profile(id: &str, name: &str, extra: &str) -> String {
    format!(
        "[[profiles]]\nid = {id:?}\nname = {name:?}\nhost = \"\"\ndatabase = \"\"\n\
         user = \"\"\nengine = \"sqlite\"\npath = DB_PATH\n{extra}\n"
    )
}

/// Extra keys of a profile an agent may see and may write through, once a
/// person approves.
const VISIBLE_RW: &str = "agent_visible = true\nmode = \"read-write\"";

fn agent_visible(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .expect("the profile was restored")
            .agent_visible
    })
}

fn saved_visible(id: &str) -> bool {
    store::peek_profiles(&[])
        .expect("profiles.toml reads back")
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .expect("the profile was saved")
        .agent_visible
}

/// The test platform has no frame loop: what the app leaves for the next
/// frame, such as a dialog handing back the keyboard, waits for this.
fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
}

fn query_tab<T>(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    read: impl FnOnce(&QueryTab, &Window, &App) -> T,
) -> T {
    cx.update(|window, cx| {
        let tab = workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .expect("a fresh profile opens on a query tab");
        read(tab, window, cx)
    })
}

fn buffer(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    query_tab(workspace, cx, |tab, _, cx| {
        tab.editor.read(cx).value().to_string()
    })
}

fn editor_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    query_tab(workspace, cx, |tab, window, cx| {
        tab.editor.focus_handle(cx).is_focused(window)
    })
}

fn grid_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    query_tab(workspace, cx, |tab, window, cx| {
        tab.results.focus_handle(cx).is_focused(window)
    })
}

fn sidebar_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        workspace
            .read(cx)
            .explorer_focus
            .contains_focused(window, cx)
    })
}

fn dialog_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| workspace.read(cx).dialog_focus.is_focused(window))
}

fn ringed_button(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.dialog_choice.get())
}

/// `selec` is no statement `sql::classify` can read, so running it puts up the
/// run-once prompt instead of reaching the database.
fn raise_run_once_prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-enter");
    settle(cx);
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.dialog_in_front.get(), Some("pending-run"));
    });
}

#[gpui::test]
fn a_query_typed_and_run_from_the_keyboard_lands_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);

        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();

        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { rows, .. } => assert_eq!(*rows, 1),
            QueryState::Failed(error) => panic!("the query failed: {error}"),
            _ => panic!("the query never completed"),
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_explained_query_lands_a_plan_that_names_its_engine_and_basis(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "CREATE TABLE t (a INTEGER)");

        cx.simulate_input("select * from t where a = 1");
        cx.simulate_keystrokes("secondary-shift-enter");
        // `settle` draws a frame, which is what renders the plan the answer
        // landed: a panic in `render_plan` fails here.
        settle(cx);

        query_tab(&workspace, cx, |tab, _, _| {
            let explained = tab
                .plan
                .as_ref()
                .expect("the Explain command landed a plan");
            assert_eq!(explained.analysis.basis, Basis::Structure);
            assert_eq!(explained.analysis.label, "SQLite · structure only");
            assert!(!explained.analysis.nodes.is_empty());
            assert!(
                explained
                    .analysis
                    .nodes
                    .iter()
                    .all(|node| node.share.is_none())
            );
            assert!(tab.showing_plan);
        });

        quit(workspace, cx);
    });
}

/// A server's answer to an explain: its columns and its rows.
type Answer = (Vec<String>, Vec<crate::db::Row>);

/// One `QUERY PLAN` cell, as Postgres answers.
fn postgres_plan(text: &str) -> Answer {
    (
        vec!["QUERY PLAN".to_string()],
        crate::db::rows(vec![vec![Some(text.to_string())]]),
    )
}

/// A MongoDB `explain` reply as the driver hands it back: one column per
/// top-level key.
fn mongo_reply(json: &str) -> Answer {
    let reply: mongodb::bson::Document =
        serde_json::from_str(json).expect("the reply is valid JSON");
    let columns = reply.keys().cloned().collect();
    let row = crate::db::Row::new(reply.values().map(|value| match value {
        mongodb::bson::Bson::String(text) => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }))
    .expect("the reply fits a row");
    (columns, vec![row])
}

#[gpui::test]
fn every_kind_of_plan_draws_its_bars_and_chips_without_panicking(cx: &mut TestAppContext) {
    use crate::{db::ExplainMode, explain::analysis::Tier, session::Explained};

    let sqlite_columns = ["id", "parent", "notused", "detail"].map(str::to_string);
    let sqlite_rows = crate::db::rows(vec![
        vec![
            Some("2".to_string()),
            Some("0".to_string()),
            Some("0".to_string()),
            Some("SCAN t".to_string()),
        ],
        vec![
            Some("5".to_string()),
            Some("2".to_string()),
            Some("0".to_string()),
            Some("USE TEMP B-TREE FOR ORDER BY".to_string()),
        ],
    ]);
    let plain = "Limit  (cost=0.00..10.00 rows=10 width=8)
  ->  Seq Scan on a  (cost=0.00..100.00 rows=1000 width=8)";
    let stats = r#"{"queryPlanner":{"winningPlan":{"stage":"LIMIT","inputStage":{"stage":"IXSCAN","indexName":"_id_"}}},"executionStats":{"nReturned":3,"executionStages":{"stage":"LIMIT","nReturned":3,"executionTimeMillisEstimate":4,"inputStage":{"stage":"IXSCAN","nReturned":3,"executionTimeMillisEstimate":1,"indexName":"_id_"}}},"ok":1}"#;

    let mut plans: Vec<(&str, Engine, ExplainMode, Answer)> = Vec::new();
    for (name, text) in [
        (
            "parallel",
            include_str!("../testdata/plan-postgres-parallel.txt"),
        ),
        (
            "cte",
            include_str!("../testdata/plan-postgres-cte-subplan.txt"),
        ),
        ("spill", include_str!("../testdata/plan-postgres-spill.txt")),
        ("large", include_str!("../testdata/plan-postgres-large.txt")),
    ] {
        plans.push((
            name,
            Engine::Postgres,
            ExplainMode::Analyze,
            postgres_plan(text),
        ));
    }
    plans.push((
        "cost",
        Engine::Postgres,
        ExplainMode::Plan,
        postgres_plan(plain),
    ));
    plans.push((
        "structure",
        Engine::Sqlite,
        ExplainMode::Plan,
        (sqlite_columns.to_vec(), sqlite_rows),
    ));
    plans.push((
        "empty",
        Engine::Postgres,
        ExplainMode::Plan,
        postgres_plan(""),
    ));
    plans.push((
        "mongo",
        Engine::MongoDb,
        ExplainMode::Analyze,
        mongo_reply(stats),
    ));

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);

        for (name, engine, mode, (columns, rows)) in plans {
            let plan = crate::explain::parse(&columns, &rows);
            let explained = Explained::new(
                engine,
                plan,
                mode,
                "select 1".to_string(),
                false,
                columns,
                rows,
            );
            if name == "parallel" {
                // A drawing test that only checks for a panic would pass on a
                // plan that is one flat colour, so the fixture has to give the
                // bars something to tell apart.
                let mut tiers: Vec<Tier> = Vec::new();
                for tier in explained.analysis.nodes.iter().filter_map(|node| node.tier) {
                    if !tiers.contains(&tier) {
                        tiers.push(tier);
                    }
                }
                assert!(tiers.len() >= 2, "{name} drew {tiers:?}");
                assert!(tiers.contains(&Tier::Quiet));
            }
            workspace.update(cx, |workspace, cx| {
                let session = &mut workspace
                    .profile_mut()
                    .expect("the SQLite profile is active")
                    .session;
                let Tab::Query(id) = session.active else {
                    panic!("a fresh profile opens on a query tab");
                };
                let tab = session
                    .query_tab_mut(id)
                    .expect("the active query tab exists");
                tab.plan = Some(explained);
                tab.showing_plan = true;
                cx.notify();
            });
            // `settle` draws a frame, which runs `render_plan` on this plan.
            settle(cx);
            query_tab(&workspace, cx, |tab, _, _| {
                assert!(tab.plan_shown().is_some(), "{name} was not in front");
            });
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_prompt_takes_the_keyboard_and_hands_it_back_once_answered(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("selec 1");
        raise_run_once_prompt(&workspace, cx);

        assert!(dialog_has_keyboard(&workspace, cx));
        assert_eq!(
            ringed_button(&workspace, cx),
            0,
            "the ring starts on Run once"
        );
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(
            buffer(&workspace, cx),
            "selec 1",
            "the buffer behind took a key"
        );

        for (keystroke, ringed) in [("left", 1), ("right", 0), ("tab", 1), ("shift-tab", 0)] {
            cx.simulate_keystrokes(keystroke);
            settle(cx);
            assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
        }

        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            buffer(&workspace, cx),
            "selec 1",
            "enter reached the buffer"
        );
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(
                matches!(tab.query, QueryState::Failed(_)),
                "Run once did not run the statement"
            );
        });
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "selec 1x");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_prompt_dismissed_from_the_keyboard_hands_it_back_to_the_editor(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("selec 1");

        for (dismiss, typed) in [("tab enter", "x"), ("escape", "y")] {
            raise_run_once_prompt(&workspace, cx);
            let before = buffer(&workspace, cx);
            cx.simulate_keystrokes(dismiss);
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.dialog_in_front.get(),
                    None,
                    "{dismiss} left it up"
                );
            });
            query_tab(&workspace, cx, |tab, _, _| {
                assert!(matches!(tab.query, QueryState::Idle), "{dismiss} ran it");
            });
            assert!(editor_has_keyboard(&workspace, cx), "after {dismiss}");
            cx.simulate_input(typed);
            settle(cx);
            assert_eq!(buffer(&workspace, cx), format!("{before}{typed}"));
        }

        quit(workspace, cx);
    });
}

/// A statement that is still running cannot be driven here: its clock ticks
/// on a timer, which the test scheduler fires at once, so nothing parks until
/// it ends. What the chord resolves to, and who handles it, is checked
/// instead, from the editor and from a grid on screen.
#[gpui::test]
fn the_cancel_chord_reaches_the_workspace_from_the_editor_and_the_grid(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(matches!(tab.query, QueryState::Complete { .. }));
        });

        let resolved: Rc<RefCell<Vec<bool>>> = Rc::default();
        let _observer = cx.update({
            let resolved = resolved.clone();
            |_, cx| {
                cx.observe_keystrokes(move |event, _, _| {
                    resolved.borrow_mut().push(
                        event
                            .action
                            .as_ref()
                            .is_some_and(|action| action.as_any().is::<CancelQuery>()),
                    );
                })
            }
        });
        let (editor, results) = query_tab(&workspace, cx, |tab, _, _| {
            (tab.editor.clone(), tab.results.clone())
        });
        for in_grid in [false, true] {
            cx.update(|window, cx| match in_grid {
                true => results.focus_handle(cx).focus(window, cx),
                false => editor.focus_handle(cx).focus(window, cx),
            });
            settle(cx);
            cx.update(|window, cx| {
                let focused = match in_grid {
                    true => results.focus_handle(cx),
                    false => editor.focus_handle(cx),
                };
                assert!(focused.is_focused(window), "in grid: {in_grid}");
                assert!(
                    window.is_action_available(&CancelQuery, cx),
                    "in grid: {in_grid}"
                );
            });
            resolved.borrow_mut().clear();
            cx.simulate_keystrokes("secondary-.");
            assert_eq!(*resolved.borrow(), [true], "in grid: {in_grid}");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_note_about_one_tab_shows_only_while_that_tab_is_in_front(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let shown = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("a profile is open").session;
                session.shown_notice().map(str::to_string)
            })
        };
        let first = workspace.update(cx, |workspace, _| {
            let session = &mut workspace.profile_mut().unwrap().session;
            session.tab_notice = Some((session.active, "cancelled here".into()));
            session.active
        });
        assert_eq!(shown(cx).as_deref(), Some("cancelled here"));

        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        assert_eq!(shown(cx), None);

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(first, window, cx)
            })
        });
        settle(cx);
        assert_eq!(shown(cx).as_deref(), Some("cancelled here"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_move_tab_chords_reorder_the_strip(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let strip = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("a profile is open").session;
                (session.strip_tabs(), session.active)
            })
        };
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        let (before, active) = strip(cx);
        assert_eq!(before.last(), Some(&active), "a new tab opens at the end");

        cx.simulate_keystrokes("ctrl-shift-pageup");
        settle(cx);
        let (after, _) = strip(cx);
        assert_eq!(after[before.len() - 2], active);

        cx.simulate_keystrokes("ctrl-shift-pagedown");
        settle(cx);
        assert_eq!(strip(cx).0, before);

        // The right end is a wall.
        cx.simulate_keystrokes("ctrl-shift-pagedown");
        settle(cx);
        assert_eq!(strip(cx).0, before);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_command_palette_opens_on_the_welcome_surface(cx: &mut TestAppContext) {
    // A `PG*` connection from the environment opens in place of the welcome
    // surface, as CI's live-test job sets.
    if ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.profile().is_none()));

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        workspace.read_with(cx, |workspace, cx| {
            let palette = workspace.palette.as_ref().expect("the palette is up");
            assert!(palette.read(cx).mode() == PaletteMode::Search(Scope::Commands));
        });
        cx.simulate_keystrokes("escape");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.palette.is_none()));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn ctrl_space_asks_for_completions_in_the_query_buffer(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let menu_open = |cx: &mut VisualTestContext| {
            query_tab(&workspace, cx, |tab, _, cx| {
                tab.editor.read(cx).completion_menu_state().open
            })
        };
        cx.simulate_input("sel");
        settle(cx);
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!menu_open(cx), "escape left the menu up");

        cx.simulate_keystrokes("ctrl-space");
        settle(cx);
        assert!(menu_open(cx));
        assert_eq!(buffer(&workspace, cx), "sel");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_keyboard_moves_between_the_buffer_and_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(
            editor_has_keyboard(&workspace, cx),
            "with no rows yet, the buffer keeps it"
        );

        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));
        query_tab(&workspace, cx, |tab, _, cx| {
            assert_eq!(tab.results.read(cx).delegate().active(), Some((0, 0)));
        });

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "select 1x");

        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_sidebar_takes_the_keyboard_and_escape_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, _| workspace.sidebar_hidden = true);

        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(!workspace.sidebar_hidden));
        assert!(sidebar_has_keyboard(&workspace, cx));

        // On the schema's row, which folds away its Tables and the table.
        cx.simulate_keystrokes("enter");
        settle(cx);
        cx.update(|_, cx| {
            let tree = &workspace.read(cx).profile().unwrap().session.explorer_tree;
            assert!(
                tree.read(cx).entry(1).is_none(),
                "enter left the schema open"
            );
        });
        assert!(sidebar_has_keyboard(&workspace, cx));

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "x");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn enter_opens_a_sidebar_row_and_its_structure_takes_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); insert into birds values ('wren');",
        );

        // The schema, its Tables, then the table.
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        // Whether the tab in front is the structure, and whether its grid has
        // the keyboard.
        let front_grid_has_keyboard = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let tab = workspace
                    .read(cx)
                    .profile()
                    .and_then(|profile| profile.session.active_object())
                    .expect("an object tab is in front");
                assert_eq!(tab.name, "birds");
                match &tab.body {
                    ObjectBody::Relation { results, .. } => {
                        (false, results.focus_handle(cx).is_focused(window))
                    }
                    ObjectBody::Structure { grid, .. } => {
                        (true, grid.focus_handle(cx).is_focused(window))
                    }
                    ObjectBody::Routine(_) => panic!("birds is a table"),
                }
            })
        };
        assert_eq!(
            front_grid_has_keyboard(cx),
            (false, true),
            "enter opened the rows"
        );

        cx.simulate_keystrokes("secondary-shift-d");
        settle(cx);
        assert_eq!(front_grid_has_keyboard(cx), (true, true));
        cx.simulate_keystrokes("down");
        settle(cx);
        cx.update(|_, cx| {
            let workspace = workspace.read(cx);
            let Some(ObjectBody::Structure { grid, .. }) = workspace
                .profile()
                .and_then(|profile| profile.session.active_object())
                .map(|tab| &tab.body)
            else {
                panic!("the structure tab is in front");
            };
            assert!(
                grid.read(cx).delegate().selection_text().is_some(),
                "the arrow reached the structure grid"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_sidebar_selects_the_row_of_the_tab_in_front(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); create table flocks (id integer);",
        );
        let selected = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let session = &workspace.read(cx).profile().unwrap().session;
                session
                    .explorer_tree
                    .read(cx)
                    .selected_item()
                    .map(|item| item.id.to_string())
            })
        };
        let active = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves
                    [&explorer::relation_row("main", name)];
                workspace.open_explorer_target(leaf.target, window, cx);
            });
            settle(cx);
        };
        let birds_row = explorer::relation_row("main", "birds");
        let flocks_row = explorer::relation_row("main", "flocks");
        assert_eq!(selected(cx), None, "a scratch query has no row");

        open("birds", cx);
        assert_eq!(selected(cx), Some(birds_row.clone()));
        let birds = active(cx);
        open("flocks", cx);
        assert_eq!(selected(cx), Some(flocks_row.clone()));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(birds_row.clone()),
            "a tab switch follows"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.new_query(&crate::actions::NewQuery, window, cx);
        });
        settle(cx);
        assert_eq!(selected(cx), None, "a new scratch query leaves no row lit");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.ask_for_name(String::new(), window, cx);
            let name = workspace.profile().unwrap().session.save_name.clone();
            name.update(cx, |input, cx| input.set_value("daily", window, cx));
            workspace.confirm_save(window, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(explorer::query_row("daily")),
            "saved, the query has a row of its own"
        );

        let daily = active(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let crate::session::Tab::Query(id) = daily else {
                panic!("the saved query is in front");
            };
            workspace.close_buffer(id, window, cx);
        });
        settle(cx);
        let fallback =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().front_row());
        assert!(fallback.is_some(), "a table tab came to the front");
        assert_eq!(selected(cx), fallback, "a close follows to the tab behind");

        // A pin repeats the table's row; selected, it keeps the selection.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().unwrap();
            profile
                .shortcuts
                .toggle_pin(explorer::Pin::Member(explorer::Member::Object(
                    explorer::ObjectRef {
                        database: None,
                        schema: "main".into(),
                        name: "birds".into(),
                        kind: explorer::ObjectKind::Relation(crate::db::RelationKind::Table),
                    },
                )));
            let id = profile.id.clone();
            workspace.refresh_explorer(&id, cx);
        });
        settle(cx);
        let pinned = workspace.update(cx, |workspace, cx| {
            let tree = workspace.profile().unwrap().session.explorer_tree.clone();
            tree.update(cx, |tree, cx| {
                let pinned = tree.entry(1).expect("the pin is listed").item().id.clone();
                assert_ne!(pinned.as_ref(), birds_row);
                tree.set_selected_index(Some(1), cx);
                pinned.to_string()
            })
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        assert_eq!(selected(cx), Some(pinned));

        // Arrowed off the tab's row, the selection outlives a rebuild that
        // lands in the background, as table sizes and folder changes do.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile().unwrap();
            let tree = profile.session.explorer_tree.clone();
            let id = profile.id.clone();
            tree.update(cx, |tree, cx| {
                let flocks = tree.index_of(&flocks_row.clone().into());
                assert!(flocks.is_some(), "flocks is listed");
                tree.set_selected_index(flocks, cx);
            });
            workspace.refresh_query_library(&id, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(flocks_row.clone()),
            "a background rebuild keeps the row the user moved to"
        );

        quit(workspace, cx);
    });
}

/// The connections tree's visible rows, as (depth, id).
fn connections_rows(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<(usize, String)> {
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        (0..)
            .map_while(|ix| tree.entry(ix))
            .map(|entry| (entry.depth(), entry.item().id.to_string()))
            .collect()
    })
}

/// Where the connections tree lists the row `id`, if it is showing.
fn connections_row(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<usize> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .connections
            .tree
            .read(cx)
            .index_of(&id.to_string().into())
    })
}

/// A row of the connections tree: whether it is open, and its children's ids
/// and labels with whether each is disabled.
type ConnectionsItem = (bool, Vec<(String, String, bool)>);

fn connections_item(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<ConnectionsItem> {
    let index = connections_row(workspace, cx, id)?;
    workspace.read_with(cx, |workspace, cx| {
        let item = workspace
            .connections
            .tree
            .read(cx)
            .entry(index)?
            .item()
            .clone();
        Some((
            item.is_expanded(),
            item.children
                .iter()
                .map(|child| {
                    (
                        child.id.to_string(),
                        child.label.to_string(),
                        child.is_disabled(),
                    )
                })
                .collect(),
        ))
    })
}

/// A click on the connections tree's row `index`.
fn click_connections_row(cx: &mut VisualTestContext, index: usize) {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-row-{index}").leak();
    let bounds = cx.debug_bounds(selector).expect("the row is drawn");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle(cx);
}

fn profile_state_of(workspace: &Workspace, id: &str) -> &'static str {
    match workspace
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .expect("the profile was restored")
        .state
    {
        ProfileState::Idle => "idle",
        ProfileState::Connecting => "connecting",
        ProfileState::Connected(_) => "connected",
        ProfileState::Failed(_) => "failed",
    }
}

fn profile_state(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> &'static str {
    workspace.read_with(cx, |workspace, _| profile_state_of(workspace, id))
}

/// The id of the connection in front, for a test already holding the workspace.
fn front_id(workspace: &Workspace) -> String {
    workspace
        .profile()
        .expect("a connection is in front")
        .id
        .clone()
}

fn front_profile(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone())
}

/// Two SQLite profiles over one file holding `birds`, `b` in the Zoo project.
/// `a` is in front, so Zoo starts folded.
fn two_connections_folded(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    launch_connections(
        cx,
        "create table birds (name text);",
        "view_mode = \"complex\"",
        &format!(
            "[[projects]]\nname = \"Zoo\"\nconnections = [\"b\"]\n{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", ""),
        ),
    )
}

/// `two_connections_folded` with Zoo opened, as a person opens it.
fn two_connections(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = two_connections_folded(cx);
    let zoo = connections_row(&workspace, cx, &project_row("Zoo")).expect("Zoo is listed");
    click_connections_row(cx, zoo);
    (workspace, cx)
}

#[gpui::test]
fn the_connections_tree_lists_every_connection_under_its_project_and_connects_one_on_expand(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections_folded(cx);
        let (a, b) = (connection_row("a"), connection_row("b"));

        // The project in front starts open and every other one folded.
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, NO_PROJECT_ROW.to_string()),
                (1, a.clone()),
                (0, project_row("Zoo")),
            ]
        );
        let zoo = connections_row(&workspace, cx, &project_row("Zoo")).unwrap();
        click_connections_row(cx, zoo);
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, NO_PROJECT_ROW.to_string()),
                (1, a),
                (0, project_row("Zoo")),
                (1, b.clone()),
            ]
        );
        assert_eq!(front_profile(&workspace, cx), "a");
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(
            connections_item(&workspace, cx, &b),
            Some((
                false,
                vec![(scoped_row("b", None, "loading"), LOADING.to_string(), true)]
            )),
            "an idle connection is folded over one row that is not a spinner"
        );

        let before = workspace.read_with(cx, |workspace, _| workspace.next_generation);
        let at = connections_row(&workspace, cx, &b).unwrap();
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1, "one connect");
        });
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");

        assert!(
            connections_row(&workspace, cx, &scoped_row("b", None, "schema-main")).is_some(),
            "the opened connection lists its schema"
        );
        assert_eq!(front_profile(&workspace, cx), "a");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn expanding_a_connection_again_while_it_connects_starts_no_second_connect(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let before = workspace.read_with(cx, |workspace, _| workspace.next_generation);

        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                for _ in 0..2 {
                    cx.emit(TreeEvent::Expanded(connection_row("b").into()));
                }
            });
        });

        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1);
        });
        assert_eq!(profile_state(&workspace, cx, "b"), "connecting");

        // A reveal of a row of a connection that is already up lands here too.
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                cx.emit(TreeEvent::Expanded(connection_row("b").into()));
            });
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1);
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_object_row_of_a_connection_behind_opens_on_that_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        // Tall enough to draw every row of both connections, which a click
        // needs: the list draws only what is in view.
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        for id in ["a", "b"] {
            // Opened and let load, the way a person would, one after the other.
            let at = connections_row(&workspace, cx, &connection_row(id)).unwrap();
            click_connections_row(cx, at);
        }
        let reveal = |id: &str, cx: &mut VisualTestContext| {
            let row = scoped_row(id, None, &explorer::relation_row("main", "birds"));
            workspace.update(cx, |workspace, cx| {
                workspace.connections.tree.update(cx, |tree, cx| {
                    let row = row.clone().into();
                    tree.reveal_item(&row, gpui::ScrollStrategy::Top, cx);
                    // A reveal alone repaints nothing; selecting does.
                    tree.set_selected_index(tree.index_of(&row), cx);
                });
            });
            settle(cx);
            row
        };
        let a_birds = reveal("a", cx);
        let b_birds = reveal("b", cx);
        assert_ne!(a_birds, b_birds);
        workspace.read_with(cx, |workspace, _| {
            for row in [&a_birds, &b_birds] {
                assert!(
                    workspace.connections.nodes.contains_key(row.as_str()),
                    "{row} is a row of its own"
                );
            }
        });
        assert_eq!(front_profile(&workspace, cx), "a");

        let at = connections_row(&workspace, cx, &b_birds).expect("the table is showing");
        click_connections_row(cx, at);

        assert_eq!(front_profile(&workspace, cx), "b");
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .and_then(|profile| profile.session.active_object())
                .expect("an object tab is in front");
            assert_eq!((tab.schema.as_str(), tab.name.as_str()), ("main", "birds"));
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_connect_that_fails_stays_on_its_own_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}{}",
                sqlite_profile("a", "Birds", ""),
                sqlite_profile("b", "Flocks", ""),
                "[[profiles]]\nid = \"c\"\nname = \"Gone\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = \"/dbdelve/no/such/file.db\"\n",
            ),
        );
        let (a, b, c) = (
            connection_row("a"),
            connection_row("b"),
            connection_row("c"),
        );
        let a_before = connections_item(&workspace, cx, &a);
        let b_before = connections_item(&workspace, cx, &b);

        let at = connections_row(&workspace, cx, &c).unwrap();
        click_connections_row(cx, at);

        let failure = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profiles.iter().find(|profile| profile.id == "c");
            match profile.map(|profile| &profile.state) {
                Some(ProfileState::Failed(message)) => message.clone(),
                _ => panic!("the connect did not fail"),
            }
        });
        assert_eq!(
            connections_item(&workspace, cx, &c),
            Some((true, vec![(scoped_row("c", None, "notice"), failure, true)]))
        );
        assert_eq!(connections_item(&workspace, cx, &a), a_before);
        assert_eq!(connections_item(&workspace, cx, &b), b_before);

        quit(workspace, cx);
    });
}

/// One DuckDB profile, `e2e`, over a file holding `t (id INTEGER)`, with the
/// connections tree drawn.
fn launch_duckdb_connection(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_duckdb(cx, "")
}

/// The same, with `extra` among the profile's scalar keys and a second file,
/// `other.duckdb` holding `u (id INTEGER)`, beside it for a statement to
/// attach. Both files are closed again before the app opens one.
fn launch_duckdb<'a>(
    cx: &'a mut TestAppContext,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_duckdb_with(cx, "", extra)
}

/// `launch_duckdb` with more `[settings]` keys.
fn launch_duckdb_with<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_duckdb_seeded(
        cx,
        settings,
        extra,
        "CREATE TABLE t (id INTEGER);",
        "CREATE TABLE u (id INTEGER);",
    )
}

/// `launch_duckdb_with`, the two files starting from the statements given
/// rather than from `t` and `u`.
fn launch_duckdb_seeded<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
    own_seed: &str,
    other_seed: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.duckdb");
    ::duckdb::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(own_seed))
        .expect("the schema must apply");
    ::duckdb::Connection::open(directory.join("other.duckdb"))
        .and_then(|connection| connection.execute_batch(other_seed))
        .expect("the second schema must apply");
    std::fs::write(
        directory.join("profiles.toml"),
        format!(
            "active = \"e2e\"\n\
             [settings]\n\
             check_for_updates = false\n\
             view_mode = \"complex\"\n\
             {settings}\n\
             [[profiles]]\n\
             id = \"e2e\"\n\
             name = \"E2E\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"duckdb\"\n\
             path = {:?}\n\
             {extra}\n",
            database.display().to_string()
        ),
    )
    .expect("profiles.toml must be writable");
    let (workspace, cx) = launch(cx);
    cx.executor().allow_parking();
    // Tall enough to draw every row, which a click needs.
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    settle(cx);
    (workspace, cx)
}

#[gpui::test]
fn a_duckdb_connection_shows_its_database_then_its_schemas(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb_connection(cx);
        let click = |id: &str, cx: &mut VisualTestContext| {
            let at = connections_row(&workspace, cx, id)
                .unwrap_or_else(|| panic!("{id} is not showing"));
            click_connections_row(cx, at);
        };

        click(&connection_row("e2e"), cx);
        let database = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .and_then(|profile| profile.active_database())
                .expect("the connection names its database")
        });
        assert_eq!(database, "e2e", "DuckDB names a file's database after it");
        let db = database_row("e2e", &database);
        let schema = scoped_row("e2e", Some(&database), "schema-main");
        let tables = scoped_row("e2e", Some(&database), "category-Tables-main");
        let t = scoped_row("e2e", Some(&database), &explorer::relation_row("main", "t"));
        assert_eq!(
            connections_item(&workspace, cx, &connection_row("e2e"))
                .map(|(_, rows)| { rows.into_iter().map(|(id, ..)| id).collect::<Vec<_>>() }),
            Some(vec![db.clone()]),
            "the database is the connection's one row"
        );
        for id in [&db, &schema, &tables] {
            click(id, cx);
        }
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, connection_row("e2e")),
                (1, db.clone()),
                (2, schema),
                (3, tables),
                (4, t.clone()),
            ]
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.connections.nodes.get(&db),
                Some(crate::sidebar::Node::Database { .. })
            ));
            // Each folder row counts what it holds, from what has loaded.
            let counts = &workspace.connections.counts;
            for id in [connection_row("e2e"), db.clone()] {
                assert_eq!(counts.get(&id), Some(&1), "{id}");
            }
            assert_eq!(
                counts.get(&scoped_row("e2e", Some(&database), "schema-main")),
                Some(&1)
            );
        });

        // The table opens on the profile, and the tree finds the row of the
        // tab in front by the id that names the database.
        click(&t, cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            let tab = profile
                .session
                .active_object()
                .expect("an object tab is in front");
            assert_eq!((tab.schema.as_str(), tab.name.as_str()), ("main", "t"));
            assert_eq!(super::sidebar::front_row_id(profile), Some(t.clone()));
        });
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        let selected = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.connections.tree.read(cx);
            tree.selected_item().map(|item| item.id.to_string())
        });
        assert_eq!(selected, Some(t));

        quit(workspace, cx);
    });
}

/// Runs `sql` in the query tab in front, and waits for the answer.
fn run_in_front(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, sql: &str) {
    cx.update(|window, cx| {
        let editor = workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .expect("a connected profile opens on a query tab")
            .editor
            .clone();
        editor.update(cx, |input, cx| input.set_value(sql, window, cx));
    });
    cx.dispatch_action(crate::actions::RunQuery);
    settle(cx);
    query_tab(workspace, cx, |tab, _, _| match &tab.query {
        QueryState::Complete { .. } => {}
        QueryState::Failed(error) => panic!("{sql} failed: {error}"),
        _ => panic!("{sql} never completed"),
    });
}

/// What `SELECT current_database()` answers on the connection of the profile
/// in front, which is the one its own tabs run on.
fn own_database(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let connection = workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .and_then(|profile| profile.connection())
            .expect("the profile is connected")
    });
    let result = connection
        .query(
            "SELECT current_database()",
            &CancelToken::default(),
            Fetch::default(),
        )
        .expect("the connection answers");
    result.rows[0]
        .iter()
        .next()
        .flatten()
        .map(str::to_string)
        .expect("a database is named")
}

/// The DuckDB profile of `launch_duckdb`, connected and holding `other`
/// attached, with the connection listed again to say so.
fn duckdb_with_other_attached(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_duckdb(cx, "mode = \"full\"");
    let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("the connection shows");
    click_connections_row(cx, at);
    attach_other(&workspace, cx);
    (workspace, cx)
}

/// Attaches `other.duckdb` to the instance the profile in front is on, and
/// lists the connection's databases again to say so.
fn attach_other(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let other = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("other.duckdb");
    run_in_front(
        workspace,
        cx,
        &format!("ATTACH '{}' AS other", other.display()),
    );
    workspace.update(cx, |workspace, cx| workspace.refresh_schema(cx));
    settle(cx);
}

#[gpui::test]
fn an_attached_duckdb_database_expands_over_a_session_of_its_own(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let (own, other) = (database_row("e2e", "e2e"), database_row("e2e", "other"));
        assert_eq!(
            connections_item(&workspace, cx, &connection_row("e2e"))
                .map(|(_, rows)| rows.into_iter().map(|(id, ..)| id).collect::<Vec<_>>()),
            Some(vec![own.clone(), other.clone()]),
            "the own database and the one attached"
        );
        assert_eq!(
            connections_item(&workspace, cx, &other),
            Some((
                false,
                vec![(
                    scoped_row("e2e", Some("other"), "loading"),
                    LOADING.to_string(),
                    true
                )]
            )),
            "folded, and nothing has opened a session"
        );
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(profile.bound.is_empty() && profile.database_catalogs.is_empty());
        });

        let click = |id: &str, cx: &mut VisualTestContext| {
            let at = connections_row(&workspace, cx, id)
                .unwrap_or_else(|| panic!("{id} is not showing"));
            click_connections_row(cx, at);
        };
        click(&other, cx);
        let schema = scoped_row("e2e", Some("other"), "schema-main");
        let tables = scoped_row("e2e", Some("other"), "category-Tables-main");
        let u = scoped_row("e2e", Some("other"), &explorer::relation_row("main", "u"));
        click(&schema, cx);
        click(&tables, cx);

        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            match profile.database_catalogs.get("other") {
                Some(CatalogState::Loaded(catalog, _)) => {
                    let names: Vec<_> = catalog
                        .schemas
                        .iter()
                        .flat_map(|schema| &schema.relations)
                        .map(|relation| relation.name.as_str())
                        .collect();
                    assert_eq!(names, ["u"]);
                }
                _ => panic!("the catalog of `other` has not loaded"),
            }
            assert_eq!(profile.active_database().as_deref(), Some("e2e"));
        });
        assert!(
            connections_row(&workspace, cx, &u).is_some(),
            "`u` is listed under the database it is in"
        );
        // The profile's own tabs are where they were.
        assert_eq!(own_database(&workspace, cx), "e2e");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn every_mode_change_reaches_the_sessions_opened_for_other_databases(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("`other` shows");
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert_eq!(profile.sessions_to_hold().len(), 2, "its own and `other`'s");
        });

        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadWrite, cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.held, [2, 2], "both were handed to the hold");
        });

        // A mode picked while a session is still opening finds nothing of it
        // to hold, so its landing drops it and opens another under the mode.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().expect("the connection is in front");
            profile.bound.clear();
            profile.database_catalogs.clear();
            workspace.expand_node(&other.clone().into(), cx);
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other", "other", "other"]);
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            assert_eq!(workspace.held.last(), Some(&1), "only its own was open");
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn expanding_a_database_twice_while_it_opens_opens_one_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        workspace.update(cx, |workspace, cx| {
            let tree = workspace.connections.tree.clone();
            tree.update(cx, |_, cx| {
                for _ in 0..2 {
                    cx.emit(TreeEvent::Expanded(other.clone().into()));
                }
            });
        });
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Opening)
            ));
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other"], "one session was opened");
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.database_catalogs.get("other"),
                Some(CatalogState::Loaded(..))
            ));
        });

        quit(workspace, cx);
    });
}

/// What the profile in front holds of the databases it opened sessions for:
/// how many sessions and how many catalogs.
fn held_of_other_databases(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (usize, usize) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a profile is in front");
        (profile.bound.len(), profile.database_catalogs.len())
    })
}

#[gpui::test]
fn disconnecting_or_reconnecting_drops_every_session_and_database_catalog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb(
            cx,
            "mode = \"full\"\n\
             [[profiles]]\n\
             id = \"z\"\n\
             name = \"Z\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"sqlite\"\n\
             path = \"z.db\"\n",
        );
        let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("it shows");
        click_connections_row(cx, at);
        attach_other(&workspace, cx);
        let other = database_row("e2e", "other");
        let expand = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                workspace.expand_node(&other.clone().into(), cx)
            });
            settle(cx);
        };
        expand(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));

        // Another profile's removal leaves this one's alone.
        workspace.update(cx, |workspace, cx| {
            let z = workspace.profiles.iter().position(|p| p.id == "z");
            workspace.remove_profile(z.expect("z is listed"), cx);
        });
        settle(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.disconnect(&Disconnect, window, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = &workspace.profiles[0];
            assert!(profile.bound.is_empty() && profile.database_catalogs.is_empty());
        });

        // Connected again, with the database attached again (an attachment
        // went with the instance), and expanded again.
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        attach_other(&workspace, cx);
        expand(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));
        workspace.update(cx, |workspace, cx| workspace.begin_connect(0, cx));
        assert_eq!(
            held_of_other_databases(&workspace, cx),
            (0, 0),
            "a connect leaves nothing of the connection it replaces"
        );
        settle(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (0, 0));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_database_that_cannot_be_opened_says_so_on_its_own_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let (own, other) = (database_row("e2e", "e2e"), database_row("e2e", "other"));
        let before = connections_item(&workspace, cx, &own);
        run_in_front(&workspace, cx, "DETACH other");

        workspace.update(cx, |workspace, cx| {
            workspace.expand_node(&other.clone().into(), cx)
        });
        settle(cx);
        let said = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("a profile is in front");
            assert!(profile.bound.is_empty(), "nothing is left half open");
            match profile.database_catalogs.get("other") {
                Some(CatalogState::Failed(message)) => message.clone(),
                _ => panic!("the failure is not on its catalog"),
            }
        });
        assert!(!said.is_empty());
        assert_eq!(
            connections_item(&workspace, cx, &other).map(|(_, rows)| rows),
            Some(vec![(
                scoped_row("e2e", Some("other"), "notice"),
                said,
                true
            )])
        );
        assert_eq!(connections_item(&workspace, cx, &own), before);

        // Another expand asks again: a failure is not a verdict.
        workspace.update(cx, |workspace, cx| {
            workspace.expand_node(&other.clone().into(), cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other", "other"]);
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn typing_in_the_connections_filter_never_connects_or_loads_anything(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        // `a` connected, with its schema opened; `b` never asked to load.
        for id in [connection_row("a"), scoped_row("a", None, "schema-main")] {
            let at = connections_row(&workspace, cx, &id).expect("the row is showing");
            click_connections_row(cx, at);
        }
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        // Typed as a person types, into the field with the keyboard: only a
        // key press tells the tree what it holds.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let filter = workspace.connections.filter.clone();
                filter.update(cx, |input, cx| input.focus(window, cx));
            });
        });
        let type_in = |text: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-a");
            match text.is_empty() {
                true => cx.simulate_keystrokes("backspace"),
                false => cx.simulate_input(text),
            }
            settle(cx);
        };
        let shown = |cx: &mut VisualTestContext| connections_rows(&workspace, cx);
        let before = shown(cx);
        let state = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.next_generation,
                    store::history("a").len(),
                    store::history("b").len(),
                )
            })
        };
        let untouched = state(cx);
        assert!(
            before
                .iter()
                .any(|(_, id)| *id == scoped_row("a", None, "schema-main"))
        );

        // `birds` is a's; `b` is idle, named Flocks, and holds nothing loaded.
        type_in("bir", cx);
        let narrowed = shown(cx);
        let birds = scoped_row("a", None, &explorer::relation_row("main", "birds"));
        assert!(narrowed.iter().any(|(_, id)| *id == birds), "{narrowed:?}");
        assert!(!narrowed.iter().any(|(_, id)| *id == connection_row("b")));
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);

        // An idle connection is kept by its own name, and still not loaded.
        type_in("flo", cx);
        assert!(
            shown(cx).iter().any(|(_, id)| *id == connection_row("b")),
            "{:?}",
            shown(cx)
        );
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);

        // Cleared, the tree is the user's again: every fold as it was.
        type_in("", cx);
        assert_eq!(shown(cx), before);
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.connections.folds_before_filter.is_none()
        }));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_connections_tree_takes_the_keyboard_and_escape_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections_folded(cx);
        assert!(!sidebar_has_keyboard(&workspace, cx));

        let selected = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let tree = workspace.connections.tree.read(cx);
                tree.selected_item().map(|item| item.id.to_string())
            })
        };

        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        assert!(sidebar_has_keyboard(&workspace, cx));
        assert_eq!(
            selected(cx),
            Some(connection_row("a")),
            "with no object open, the connection in front is the row"
        );
        // An arrow only moves a tree that holds the keyboard.
        cx.simulate_keystrokes("down");
        settle(cx);
        assert_eq!(selected(cx), Some(project_row("Zoo")));

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!sidebar_has_keyboard(&workspace, cx));
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

/// A key pressed and let go: a focused button clicks on the release, which
/// `simulate_keystrokes` never sends.
fn press(cx: &mut VisualTestContext, key: &str) {
    let keystroke = gpui::Keystroke::parse(key).expect("a key the test names");
    cx.simulate_event(gpui::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.simulate_event(gpui::KeyUpEvent { keystroke });
    settle(cx);
}

fn in_dialog(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| workspace.read(cx).dialog_focus.contains_focused(window, cx))
}

fn front_dialog(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<&'static str> {
    workspace.read_with(cx, |workspace, _| workspace.dialog_in_front.get())
}

/// What decides which surface the window draws, for an assertion that a
/// dialog is up: only the workspace surface has a layer for most of them.
fn surface(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    workspace.read_with(cx, |workspace, _| {
        format!(
            "form open: {}, profile in front: {}, profiles: {}, visibility prompt: {:?}",
            workspace.form.is_some(),
            workspace.profile().is_some(),
            workspace.profiles.len(),
            workspace.agent_visibility_prompt,
        )
    })
}

fn palette_mode(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<PaletteMode> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .palette
            .as_ref()
            .map(|palette| palette.read(cx).mode())
    })
}

#[gpui::test]
fn tab_walks_the_import_dialog_and_its_buttons_answer_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let file = store::dbdelve_directory().unwrap().join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile().unwrap().id.clone();
            workspace.open_import_file(
                profile,
                None,
                "main".into(),
                "birds".into(),
                file,
                window,
                cx,
            );
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        assert!(dialog_has_keyboard(&workspace, cx));
        let format = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                dialog.map(|dialog| dialog.options.format)
            })
        };

        // Change file…, then the format's CSV and TSV.
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        assert!(in_dialog(&workspace, cx));
        assert!(!dialog_has_keyboard(&workspace, cx));
        press(cx, "space");
        assert_eq!(format(cx), Some(crate::import_rows::Format::Tsv));

        // JSON, the header box, then the delimiter's list, which `escape` closes
        // without taking the dialog down with it.
        let in_menu = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("PopupMenu"))
            })
        };
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        press(cx, "space");
        assert!(in_menu(cx), "space opened no list");
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!in_menu(cx));
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        // Round and round, never out of the dialog nor into its preview.
        let preview = workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            dialog.unwrap().preview.clone()
        });
        for _ in 0..40 {
            cx.simulate_keystrokes("tab");
            settle(cx);
            assert!(in_dialog(&workspace, cx));
            cx.update(|window, cx| assert!(!preview.focus_handle(cx).is_focused(window)));
        }

        // The file is checked a moment after the last change to how it is read.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        let skip_lines = workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            assert_eq!(super::import_dialog::import_blocked(dialog.unwrap()), None);
            dialog.unwrap().inputs.skip_lines.clone()
        });
        cx.update(|window, cx| skip_lines.focus_handle(cx).focus(window, cx));
        press(cx, "enter");
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let editing = matches!(dialog.unwrap().phase, crate::session::ImportPhase::Editing);
            assert!(editing, "enter in a field started the import");
        });

        // From the dialog's own focus, back to Import and then Cancel.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.dialog_focus.focus(window, cx)
        });
        cx.simulate_keystrokes("shift-tab shift-tab");
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_and_paste_in_a_table_carry_rows_into_the_import_dialog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text, note text);
             insert into birds values (1, 'wren', null), (2, 'robin', '');",
        );
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let clipboard = |cx: &mut VisualTestContext| {
            let item = cx.read_from_clipboard().expect("something was copied");
            let metadata = item.entries().iter().find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.clone(),
                _ => None,
            });
            (item.text().unwrap_or_default(), metadata)
        };
        let gutter = |row: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = format!("row-number-{row}").leak();
            cx.debug_bounds(selector).expect("the row is drawn")
        };

        let first = gutter(0, cx).center();
        cx.simulate_click(first, gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");
        let (text, metadata) = clipboard(cx);
        assert_eq!(text, "id\tname\tnote\n1\twren\t\n");
        assert!(metadata.is_some(), "a row copy carries its rows");

        let second = gutter(1, cx).center();
        cx.simulate_click(second, gpui::Modifiers::shift());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");
        let (text, metadata) = clipboard(cx);
        assert_eq!(text, "id\tname\tnote\n1\twren\t\n2\trobin\t\"\"\n");
        let pasted = crate::import_rows::pasted(&text, metadata.as_deref(), false);
        assert_eq!(
            pasted.map(|(format, bytes, _)| (format, String::from_utf8(bytes).unwrap())),
            Some((
                crate::import_rows::Format::Json,
                "{\"id\":\"1\",\"name\":\"wren\",\"note\":null}\n\
                 {\"id\":\"2\",\"name\":\"robin\",\"note\":\"\"}"
                    .into()
            ))
        );

        // A cell of the first row, clicked: that cell alone, as Copy Cell has it.
        let cell = gutter(0, cx).right_center() + gpui::point(gpui::px(12.), gpui::px(0.));
        cx.simulate_click(cell, gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("right secondary-c");
        settle(cx);
        assert_eq!(clipboard(cx), ("wren".into(), None));

        // An open input keeps both keystrokes for its own text.
        let resolved: Rc<RefCell<Vec<Option<&'static str>>>> = Rc::default();
        let _observer = cx.update({
            let resolved = resolved.clone();
            |_, cx| {
                cx.observe_keystrokes(move |event, _, _| {
                    let action = event.action.as_ref().map(|action| action.name());
                    resolved.borrow_mut().push(action);
                })
            }
        });
        cx.simulate_keystrokes("enter");
        settle(cx);
        resolved.borrow_mut().clear();
        cx.simulate_keystrokes("secondary-c secondary-v");
        settle(cx);
        assert_eq!(
            *resolved.borrow(),
            [Some("input::Copy"), Some("input::Paste")]
        );
        assert_eq!(front_dialog(&workspace, cx), None);
        cx.simulate_keystrokes("escape");
        settle(cx);

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("name\nfinch\n".into()));
        cx.simulate_keystrokes("secondary-v");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let dialog = dialog.expect("the dialog is open");
            assert_eq!(dialog.source.name(), "Clipboard");
            assert_eq!(&*dialog.bytes, b"name\nfinch\n");
        });

        // A value a paste could not type holds the import up where it is.
        let profile =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        workspace.update(cx, |workspace, cx| {
            let session = &mut workspace.profile_mut().unwrap().session;
            let dialog = session.import_rows.as_mut().unwrap();
            dialog.options.format = crate::import_rows::Format::Json;
            dialog.bytes = b"{\"name\":\"finch\"}".as_slice().into();
            dialog.source =
                crate::session::ImportSource::Clipboard(vec![crate::import_rows::Problem {
                    row: Some(0),
                    line: Some(1),
                    column: Some(0),
                    message: "unreadable".into(),
                }]);
            workspace.recheck_import(&profile, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let Some(Ok(prepared)) = &dialog.unwrap().prepared else {
                panic!("the paste was not checked");
            };
            let problems = &prepared.checked.problems;
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.message == "unreadable"),
                "{problems:?}"
            );
        });

        // A paste read after another import was asked for, or after a dialog
        // opened, opens nothing.
        let take = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, _| {
                workspace.profile_mut().unwrap().session.import_rows.take()
            })
        };
        let shown = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                dialog.map(|dialog| dialog.bytes.to_vec())
            })
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("late\n1\n".into()));
        let opened = take(cx);
        let opened_bytes = opened.as_ref().map(|dialog| dialog.bytes.to_vec());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            workspace.profile_mut().unwrap().session.import_rows = opened;
        });
        settle(cx);
        assert_eq!(shown(cx), opened_bytes);

        take(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            workspace.import_rows_into(None, "main".into(), "birds".into(), window, cx);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        // Nor one read while the profile moved to another database, whose
        // table of the same name is not the one asked for.
        let moved = |workspace: &mut Workspace| workspace.profile_mut().unwrap().generation += 1;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            moved(workspace);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        // A file read is held to both the same way.
        let file = store::dbdelve_directory().unwrap().join("late.csv");
        std::fs::write(&file, "id\n7\n").unwrap();
        let read_file =
            |workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>| {
                let profile = workspace.profile().unwrap().id.clone();
                workspace.open_import_file(
                    profile,
                    None,
                    "main".into(),
                    "birds".into(),
                    file.clone(),
                    window,
                    cx,
                );
            };
        workspace.update_in(cx, |workspace, window, cx| {
            read_file(workspace, window, cx);
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
        });
        settle(cx);
        assert_eq!(shown(cx), Some(b"late\n1\n".to_vec()));
        take(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            read_file(workspace, window, cx);
            moved(workspace);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn rows_copied_from_one_table_paste_into_another_exactly(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, note text, big integer, ratio real, body text, pic blob);
             insert into birds values
               (1, null, 9223372036854775807, 0.1, 'a	b \"q\" ''s''
ünï ✓', x'00FF10'),
               (2, '', -9007199254740993, 1.5e300, 'plain', null),
               (3, 'NULL', 0, -2.5, '', x'');
             create table nests (pic blob, body text, ratio real, big integer, note text, id integer primary key);",
        );
        cx.executor().allow_parking();
        let read = |table: &str| -> Vec<Vec<String>> {
            // Not `pic`: Import Rows writes the grid's `x'00FF10'` for a blob
            // as text, as it does reading the same value from a file.
            let columns = ["id", "note", "big", "ratio", "body"]
                .map(|c| format!("typeof({c}) || ' ' || quote({c})"))
                .join(", ");
            let db = rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
                .unwrap();
            let mut statement = db
                .prepare(&format!("select {columns} from {table} order by id"))
                .unwrap();
            statement
                .query_map([], |row| (0..5).map(|i| row.get::<_, String>(i)).collect())
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-p");
            settle(cx);
            cx.simulate_input(name);
            cx.simulate_keystrokes("enter");
            settle(cx);
            let front = workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                session.active_object().map(|tab| tab.name.clone())
            });
            assert_eq!(front.as_deref(), Some(name));
        };
        let paste_and_import = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-v");
            settle(cx);
            assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
            cx.executor()
                .advance_clock(std::time::Duration::from_secs(1));
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                assert_eq!(super::import_dialog::import_blocked(dialog.unwrap()), None);
            });
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.dialog_focus.focus(window, cx)
            });
            cx.simulate_keystrokes("shift-tab");
            press(cx, "enter");
            for _ in 0..100 {
                if front_dialog(&workspace, cx).is_none() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                settle(cx);
            }
            assert_eq!(
                front_dialog(&workspace, cx),
                None,
                "the import never finished"
            );
        };

        open("birds", cx);
        let gutter = |row: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = format!("row-number-{row}").leak();
            cx.debug_bounds(selector).expect("the row is drawn")
        };
        let first = gutter(0, cx).center();
        cx.simulate_click(first, gpui::Modifiers::default());
        let last = gutter(2, cx).center();
        cx.simulate_click(last, gpui::Modifiers::shift());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");

        // The way a mouse opens it: an empty table, its keyboard on nothing
        // the grid draws.
        let nests = explorer_row(&workspace, "relation-main\0nests", cx).center();
        cx.simulate_click(nests, gpui::Modifiers::default());
        settle(cx);
        paste_and_import(cx);
        assert_eq!(read("nests"), read("birds"));

        // A spreadsheet's cells, which carry no types and no NULL of their own.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "id\tbody\tnote\tbig\n10\tfrom a sheet\t\t42\n".into(),
        ));
        paste_and_import(cx);
        assert_eq!(
            read("nests").last().unwrap(),
            &[
                "integer 10",
                "null NULL",
                "integer 42",
                "null NULL",
                "text 'from a sheet'",
            ]
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_import_question_picks_its_project_from_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.pending_import = Some(super::PendingImport {
                source: crate::import::Source::TablePlus,
                report: crate::import::Report::default(),
                fresh: 1,
                project: Some("Imported from TablePlus".into()),
            });
            cx.notify();
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-choice"));
        assert_eq!(ringed_button(&workspace, cx), 0);

        cx.simulate_keystrokes("left left");
        settle(cx);
        assert_eq!(
            ringed_button(&workspace, cx),
            2,
            "the ring reaches the project"
        );
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::ImportProject));

        cx.simulate_input("No project");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_none());
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.pending_import.as_ref().unwrap().project, None);
        });
        assert_eq!(front_dialog(&workspace, cx), Some("import-choice"));
        assert!(dialog_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_ring_reaches_dont_ask_again_and_enter_or_space_ticks_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::Full, cx)
        });
        cx.simulate_input("delete from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));
        assert_eq!(ringed_button(&workspace, cx), 0, "the ring starts on Run");
        let dont_ask = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                session.pending_run.as_ref().map(|pending| pending.dont_ask)
            })
        };

        for (keystroke, ringed, ticked) in [
            // A space typed on as the prompt goes up presses nothing.
            ("space", 0, false),
            ("left left", 2, false),
            ("enter", 2, true),
            ("space", 2, false),
            ("enter", 2, true),
            ("tab", 1, true),
            ("tab", 0, true),
            ("tab", 2, true),
        ] {
            cx.simulate_keystrokes(keystroke);
            settle(cx);
            assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
            assert_eq!(dont_ask(cx), Some(ticked), "after {keystroke}");
            assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));
        }

        cx.simulate_keystrokes("right right enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(
                profile
                    .confirmed
                    .contains(&crate::sql::Destructive::UnfilteredDelete),
                "Run ran with the box ticked"
            );
        });

        quit(workspace, cx);
    });
}

/// Birds and the sightings that reference them, with the keyboard on the
/// first bird's key.
fn launch_on_a_referenced_key(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_on_sqlite_holding(
        cx,
        "create table birds (id integer primary key, name text);
         create table sightings (id integer primary key, bird integer references birds(id));
         insert into birds values (1, 'wren');
         insert into sightings values (1, 1);",
    );
    cx.simulate_keystrokes("secondary-k e");
    settle(cx);
    cx.simulate_keystrokes("down down enter");
    settle(cx);
    assert_eq!(front_object(&workspace, cx).as_deref(), Some("birds"));
    cx.simulate_keystrokes("down");
    settle(cx);
    (workspace, cx)
}

fn front_object(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| {
        let session = &workspace.profile().unwrap().session;
        session.active_object().map(|tab| tab.name.clone())
    })
}

fn ask_for_references(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-shift-p");
    settle(cx);
    cx.simulate_input("Show references");
    cx.simulate_keystrokes("enter");
    settle(cx);
}

#[gpui::test]
fn show_references_lists_the_referencing_tables_in_the_palette(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_a_referenced_key(cx);
        ask_for_references(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::References));

        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_none());
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("sightings"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn references_listed_for_one_key_never_open_from_another(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text);
             create table flocks (id integer primary key);
             create table members (id integer primary key, flock integer references flocks(id));
             create table sightings (id integer primary key, bird integer references birds(id));
             insert into birds values (1, 'wren');
             insert into flocks values (1);
             insert into members values (1, 1);
             insert into sightings values (1, 1);",
        );
        let active = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };
        // Flocks' key under the keyboard, then birds'.
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down down enter");
        settle(cx);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("flocks"));
        cx.simulate_keystrokes("down");
        settle(cx);
        let flocks = active(cx);
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("up enter");
        settle(cx);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("birds"));
        cx.simulate_keystrokes("down");
        settle(cx);
        let birds = active(cx);

        // Picked once flocks is in front again: its own reference sits at the
        // same index, and opening it would read members with flocks' key.
        ask_for_references(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::References));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(flocks, window, cx);
        });
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(active(cx), flocks);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("flocks"));

        // Answered once flocks is in front again.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        let checks_before = workspace.read_with(cx, |workspace, _| workspace.reference_checks);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_references(&crate::actions::ShowReferences, window, cx);
            workspace.activate_tab(flocks, window, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.reference_checks,
                checks_before + 1,
                "no lookup started, so nothing stale was tested"
            );
        });
        assert!(
            palette_mode(&workspace, cx).is_none(),
            "a stale answer was listed"
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.reference_popup.is_none())
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_closes_the_reference_arrows_list(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.reference_popup = Some(super::ReferencePopup {
                at: Some(gpui::point(gpui::px(10.), gpui::px(10.))),
                source: super::ReferenceSource {
                    profile: String::new(),
                    tab: 0,
                    column: String::new(),
                    value: String::new(),
                    referenced_by: Vec::new(),
                },
                choices: Some(Vec::new()),
                check: 0,
            });
            cx.notify();
        });
        settle(cx);
        cx.simulate_keystrokes("escape");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.reference_popup.is_none())
        });
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn tab_reaches_the_default_sorting_choice_in_settings(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_keystrokes("secondary-,");
        settle(cx);
        cx.simulate_input("Default sorting");
        settle(cx);
        let client_sort =
            |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.settings.client_sort);
        assert!(!client_sort(cx));

        // Server, then Client.
        cx.simulate_keystrokes("tab tab");
        settle(cx);
        press(cx, "space");
        assert!(client_sort(cx));

        quit(workspace, cx);
    });
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn an_existing_cmd_m_binding_can_be_recaptured_in_settings(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "[settings]\ncheck_for_updates = false\n\
             [settings.custom_keybindings]\nshow_messages = \"secondary-m\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        cx.simulate_keystrokes("secondary-,");
        settle(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.start_rebind("show_messages", window, cx);
            });
        });
        settle(cx);
        cx.simulate_keystrokes("cmd-m");
        settle(cx);
        let saved = store::peek_profiles(&[])
            .unwrap()
            .settings
            .unwrap()
            .custom_keybindings
            .unwrap();
        assert_eq!(
            saved.get("show_messages").map(String::as_str),
            Some("cmd-m")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_selection_failing_part_way_says_what_it_already_committed(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "CREATE TABLE t (a INTEGER);");
        cx.executor().allow_parking();
        cx.simulate_input(
            "insert into t values (1); insert into t values (2); \
             select * from nope; insert into t values (3)",
        );
        cx.simulate_keystrokes("secondary-a secondary-enter");
        settle(cx);
        let failed = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("a profile is active");
            assert!(profile.session.queue_failure.is_some());
            let tab = profile.session.active_query_tab().expect("a query tab");
            let last = tab.queue.as_ref().and_then(|queue| queue.done.last());
            match &last.expect("a queue with results").state {
                QueryState::Failed(error) => error.message.clone(),
                _ => panic!("the third statement did not fail"),
            }
        });
        assert!(
            failed.ends_with("\n\n2 statements before it were already committed."),
            "{failed}"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn stepping_to_the_next_result_keeps_the_keyboard_in_the_grid(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.executor().allow_parking();
        cx.simulate_input("select 1; select 2");
        cx.simulate_keystrokes("secondary-a secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        let shown = |cx: &mut VisualTestContext| {
            query_tab(&workspace, cx, |tab, window, cx| {
                (
                    tab.switcher_selected(),
                    tab.shown().1.focus_handle(cx).is_focused(window),
                )
            })
        };
        assert_eq!(shown(cx), (Some(0), true));

        cx.dispatch_action(crate::actions::NextResult);
        settle(cx);
        assert_eq!(shown(cx), (Some(1), true));
        assert!(!editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_switches_who_sorts_the_view(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1 as a");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let client = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                crate::views::view_sorts_on_client(workspace.profile().unwrap(), cx)
            })
        };
        assert_eq!(client(cx), Some(false));

        for (row, after) in [("Sort on the client", true), ("Sort on the server", false)] {
            cx.simulate_keystrokes("secondary-shift-p");
            settle(cx);
            cx.simulate_input(row);
            cx.simulate_keystrokes("enter");
            settle(cx);
            assert_eq!(client(cx), Some(after), "after {row}");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_copies_what_the_explorer_menu_copies(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);

        for (row, copied) in [
            ("Copy SELECT (Top 100)", "SELECT"),
            ("Copy DROP", "DROP TABLE"),
        ] {
            cx.simulate_keystrokes("secondary-shift-p");
            settle(cx);
            cx.simulate_input(row);
            cx.simulate_keystrokes("enter");
            settle(cx);
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert!(
                text.contains(copied) && text.contains("birds"),
                "{row}: {text}"
            );
        }
        // Copied, never run.
        let tables = rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| {
                db.query_row("select count(*) from sqlite_master", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .unwrap();
        assert_eq!(tables, 1);

        quit(workspace, cx);
    });
}

/// What the `Show DDL` dialog holds, if it is up.
fn ddl_text(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| {
        workspace.ddl_view.as_ref().map(|view| match &view.text {
            super::DdlText::Loading => "loading".to_string(),
            super::DdlText::Ready(text) => text.clone(),
            super::DdlText::Failed(error) => format!("failed: {error}"),
        })
    })
}

#[gpui::test]
fn show_ddl_opens_a_read_only_view_of_a_tables_definition_and_hands_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let before = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            (session.queries.len(), session.history.len())
        });
        let typed = buffer(&workspace, cx);
        assert!(editor_has_keyboard(&workspace, cx));

        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(None, "main".into(), "birds".into(), RelationKind::Table, cx)
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert!(dialog_has_keyboard(&workspace, cx));
        let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
        assert!(text.contains("CREATE TABLE birds"), "{text}");
        assert!(
            cx.debug_bounds("show-ddl-text").is_some(),
            "the definition is drawn"
        );
        // A dialog, not a buffer: nothing a tab holds changed.
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!((session.queries.len(), session.history.len()), before);
        });
        assert_eq!(buffer(&workspace, cx), typed);

        press(cx, "escape");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(ddl_text(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_relation_menu_offers_show_ddl_where_the_target_copies_ddl(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        // Open Data, Open Structure, Import Rows, Export, then the copies: Copy
        // Name, then Copy DDL as the sixth item and Show DDL the one after it.
        for (downs, copied, dialog) in [(6, true, None), (7, false, Some("show-ddl"))] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
            let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
            cx.simulate_event(gpui::MouseDownEvent {
                position: row,
                modifiers: gpui::Modifiers::default(),
                button: gpui::MouseButton::Right,
                click_count: 1,
                first_mouse: false,
            });
            settle(cx);
            cx.update(|window, _| {
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu")),
                    "the row's menu is up"
                )
            });
            for _ in 0..downs {
                cx.simulate_keystrokes("down");
            }
            press(cx, "enter");
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(
                text.contains("CREATE TABLE birds"),
                copied,
                "item {downs}: {text}"
            );
            assert_eq!(front_dialog(&workspace, cx), dialog, "item {downs}");
            if dialog.is_some() {
                press(cx, "escape");
            }
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_empty_definition_says_so_and_offers_nothing_to_copy(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let put_up = |text: super::DdlText, cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let profile = workspace
                    .profile()
                    .expect("a connection is in front")
                    .id
                    .clone();
                workspace.ddl_view = Some(super::DdlView {
                    profile,
                    database: None,
                    schema: "main".into(),
                    name: "birds".into(),
                    kind: Some(RelationKind::Table),
                    title: "Table main.birds".into(),
                    text,
                    diagram: None,
                    showing_diagram: false,
                    request: 1,
                });
                cx.notify();
            });
            settle(cx);
        };
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };

        put_up(super::DdlText::Ready(String::new()), cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert!(cx.debug_bounds("show-ddl-empty").is_some());
        assert!(cx.debug_bounds("show-ddl-text").is_none());
        // Neither the button nor the ring puts anything on the clipboard.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        let copy = cx
            .debug_bounds("show-ddl-1")
            .expect("Copy is drawn")
            .center();
        cx.simulate_click(copy, gpui::Modifiers::default());
        settle(cx);
        press(cx, "left");
        assert_eq!(ringed_button(&workspace, cx), 1, "the ring is on Copy");
        press(cx, "enter");
        assert_eq!(clipboard(cx), "before");
        assert_eq!(
            front_dialog(&workspace, cx),
            Some("show-ddl"),
            "Copy leaves the dialog up"
        );

        // The same click copies a definition that is there.
        put_up(
            super::DdlText::Ready("CREATE TABLE birds (name text);".into()),
            cx,
        );
        let copy = cx
            .debug_bounds("show-ddl-1")
            .expect("Copy is drawn")
            .center();
        cx.simulate_click(copy, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(clipboard(cx), "CREATE TABLE birds (name text);");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn show_ddl_reads_another_databases_definition_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        let shown = |database: Option<&str>, cx: &mut VisualTestContext| {
            let database = database.map(str::to_string);
            workspace.update(cx, |workspace, cx| {
                workspace.show_ddl(database, "main".into(), "t".into(), RelationKind::Table, cx)
            });
            settle(cx);
            let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
            press(cx, "escape");
            text
        };
        let other = shown(Some("other"), cx);
        assert!(
            other.contains("extra"),
            "`other`'s `t`, which has `extra`: {other}"
        );
        let own = shown(None, cx);
        assert!(
            own.contains("CREATE TABLE t") && !own.contains("extra"),
            "the connection's own `t`: {own}"
        );
        // Asking for the own database by name is the connection's own `t` too.
        let named = shown(Some("e2e"), cx);
        assert_eq!(named, own);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_ddl_reads_another_databases_definition_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        for (database, has_extra) in [(Some("other"), true), (None, false)] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
            workspace.update(cx, |workspace, cx| {
                workspace.copy_ddl(
                    database.map(str::to_string),
                    "main".into(),
                    "t".into(),
                    RelationKind::Table,
                    cx,
                )
            });
            settle(cx);
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert!(text.contains("CREATE TABLE t"), "{database:?}: {text}");
            assert_eq!(text.contains("extra"), has_extra, "{database:?}: {text}");
        }
        quit(workspace, cx);
    });
}

/// Clicks the dialog's `SQL` or `Diagram` choice.
fn choose_in_ddl_dialog(cx: &mut VisualTestContext, choice: &'static str) {
    let at = cx
        .debug_bounds(choice)
        .unwrap_or_else(|| panic!("{choice} is drawn"))
        .center();
    cx.simulate_click(at, gpui::Modifiers::default());
    settle(cx);
}

#[gpui::test]
fn the_diagram_shows_a_tables_keys_and_targets_and_sends_no_new_statement(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table owners (id integer primary key); \
             create table birds (id integer primary key, owner_id integer references owners(id));",
        );
        let history = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.history.len()
            })
        };
        let before = history(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(None, "main".into(), "birds".into(), RelationKind::Table, cx)
        });
        settle(cx);
        assert!(cx.debug_bounds("show-ddl-text").is_some(), "SQL first");
        assert!(cx.debug_bounds("diagram-table-birds").is_none());

        choose_in_ddl_dialog(cx, "show-ddl-diagram-choice");
        for drawn in [
            "diagram-table-birds",
            "diagram-pk-id",
            "diagram-fk-owner_id",
            "diagram-target-owners",
        ] {
            assert!(cx.debug_bounds(drawn).is_some(), "{drawn} is drawn");
        }
        assert!(cx.debug_bounds("diagram-pk-owner_id").is_none());
        assert!(cx.debug_bounds("show-ddl-text").is_none());
        assert_eq!(history(cx), before, "the diagram ran nothing of its own");

        choose_in_ddl_dialog(cx, "show-ddl-sql-choice");
        assert!(cx.debug_bounds("show-ddl-text").is_some());
        assert!(cx.debug_bounds("diagram-table-birds").is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_diagram_choice_is_offered_only_for_an_object_with_columns(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(None, "main".into(), "birds".into(), RelationKind::Table, cx)
        });
        settle(cx);
        assert!(cx.debug_bounds("show-ddl-diagram-choice").is_some());
        assert!(cx.debug_bounds("show-ddl-sql-choice").is_some());

        // An object with no structure, as a sequence is.
        workspace.update(cx, |workspace, cx| {
            let view = workspace.ddl_view.as_mut().expect("the dialog is up");
            view.kind = None;
            cx.notify();
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert!(cx.debug_bounds("show-ddl-diagram-choice").is_none());
        assert!(cx.debug_bounds("show-ddl-sql-choice").is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_diagram_reads_another_databases_structure_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        let columns = |database: Option<&str>, cx: &mut VisualTestContext| {
            let database = database.map(str::to_string);
            workspace.update(cx, |workspace, cx| {
                workspace.show_ddl(database, "main".into(), "t".into(), RelationKind::Table, cx)
            });
            settle(cx);
            choose_in_ddl_dialog(cx, "show-ddl-diagram-choice");
            let columns = workspace.read_with(cx, |workspace, _| {
                match workspace
                    .ddl_view
                    .as_ref()
                    .and_then(|view| view.diagram.as_ref())
                {
                    Some(super::DiagramState::Ready(diagram)) => diagram
                        .table
                        .columns
                        .iter()
                        .map(|column| column.name.clone())
                        .collect::<Vec<_>>(),
                    _ => panic!("the diagram has not loaded"),
                }
            });
            press(cx, "escape");
            columns
        };
        assert_eq!(columns(Some("other"), cx), ["name", "id", "extra"]);
        assert_eq!(columns(None, cx), ["name", "id"]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_empty_table_opened_from_the_sidebar_keeps_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let rows = workspace.read_with(cx, |workspace, cx| {
            let session = &workspace.profile().unwrap().session;
            session
                .active_results()
                .map(|grid| grid.read(cx).delegate().total_rows())
        });
        assert_eq!(rows, Some(0));
        cx.update(|window, _| assert!(!window.context_stack().is_empty(), "no dispatch path"));

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_some());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn tab_onto_a_welcome_project_shows_its_rename_and_delete(cx: &mut TestAppContext) {
    if ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "[settings]\ncheck_for_updates = false\n[[projects]]\nname = \"Birds\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        settle(cx);
        let buttons_have_keyboard = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let buttons = workspace.read(cx).welcome_project_focus.borrow()[0].clone();
                buttons.contains_focused(window, cx)
            })
        };
        assert!(!buttons_have_keyboard(cx));

        let mut reached = false;
        for _ in 0..30 {
            cx.simulate_keystrokes("tab");
            settle(cx);
            if buttons_have_keyboard(cx) {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab never reached the project's buttons");
        cx.update(|window, cx| {
            let buttons = workspace.read(cx).welcome_project_focus.borrow()[0].clone();
            assert!(
                !buttons.is_focused(window),
                "Rename or Delete has it, not their holder"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_clicked_box_leaves_the_ring_in_charge_of_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::Full, cx)
        });
        cx.simulate_input("delete from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));

        let box_at = cx
            .debug_bounds("pending-run-2")
            .expect("the box is drawn in the ring")
            .center();
        cx.simulate_click(box_at, gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert!(session.pending_run.as_ref().unwrap().dont_ask);
        });

        // Round to Run, which `enter` presses rather than the box it would tick.
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        assert_eq!(ringed_button(&workspace, cx), 0);
        assert!(dialog_has_keyboard(&workspace, cx));
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None, "enter pressed nothing");
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(
                profile
                    .confirmed
                    .contains(&crate::sql::Destructive::UnfilteredDelete)
            );
        });

        quit(workspace, cx);
    });
}

/// A session moved on behind the modeless save panel still gets its rows, under
/// every column they hold, rather than what its connection now says of a table
/// of that name.
#[gpui::test]
fn a_session_changed_behind_the_save_panel_is_not_asked_about_the_table(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let database = directory.join("e2e.duckdb");
        ::duckdb::Connection::open(&database)
            .and_then(|connection| {
                connection.execute_batch(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, \
                     twice INTEGER GENERATED ALWAYS AS (id * 2) VIRTUAL); \
                     INSERT INTO t (id) VALUES (1);",
                )
            })
            .expect("the schema must apply");
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\n\
                 check_for_updates = false\n\
                 [[profiles]]\n\
                 id = \"e2e\"\n\
                 name = \"E2E\"\n\
                 host = \"\"\n\
                 database = \"\"\n\
                 user = \"\"\n\
                 engine = \"duckdb\"\n\
                 path = {:?}\n",
                database.display().to_string()
            ),
        )
        .expect("profiles.toml must be writable");
        let (workspace, cx) = launch(cx);
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.config.engine() == Engine::DuckDb)
                .expect("the saved DuckDB profile is restored");
            workspace.activate(index, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("SELECT * FROM t");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        let mut export = |changed: bool, name: &str| {
            let path = directory.join(name);
            workspace.update(cx, |workspace, cx| {
                workspace.export_results(crate::export::FileAs::Inserts, cx);
            });
            settle(cx);
            if changed {
                workspace.update(cx, |workspace, _| {
                    workspace.profile_mut().unwrap().generation += 1;
                });
            }
            let chosen = path.clone();
            cx.simulate_new_path_selection(|_| Some(chosen));
            for _ in 0..500 {
                settle(cx);
                if path.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::fs::read_to_string(&path).expect("the file is written")
        };

        assert!(!export(false, "same.sql").contains("twice"));
        assert!(export(true, "changed.sql").contains("twice"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_export_question_takes_the_keyboard_and_hands_it_back_when_dismissed(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        // Past the default row limit, so the tab holds the first rows only.
        cx.simulate_input(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5000) \
             SELECT x FROM n",
        );
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(
                matches!(tab.query, QueryState::Complete { .. }),
                "the query never completed"
            );
        });

        for (dismiss, typed) in [("escape", " "), ("tab tab enter", "x")] {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .export_results(crate::export::FileAs::Rows(crate::export::Format::Csv), cx);
            });
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(workspace.dialog_in_front.get(), Some("pending-export"));
            });
            assert!(dialog_has_keyboard(&workspace, cx));
            assert_eq!(
                ringed_button(&workspace, cx),
                0,
                "the ring starts on Export All Rows"
            );
            for (keystroke, ringed) in [
                ("left", 1),
                ("left", 2),
                ("left", 0),
                ("right", 2),
                ("right", 1),
                ("right", 0),
            ] {
                cx.simulate_keystrokes(keystroke);
                settle(cx);
                assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
            }

            let before = buffer(&workspace, cx);
            cx.simulate_keystrokes(dismiss);
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.dialog_in_front.get(),
                    None,
                    "{dismiss} left it up"
                );
                let session = &workspace.profile().unwrap().session;
                assert!(session.exporting.is_none(), "{dismiss} started an export");
            });
            assert!(editor_has_keyboard(&workspace, cx), "after {dismiss}");
            cx.simulate_input(typed);
            settle(cx);
            assert_eq!(buffer(&workspace, cx), format!("{before}{typed}"));
        }

        quit(workspace, cx);
    });
}

fn relation_names(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        let Some(CatalogState::Loaded(catalog, _)) =
            workspace.profile().map(|profile| &profile.catalog)
        else {
            panic!("the catalog should have loaded");
        };
        catalog
            .schemas
            .iter()
            .flat_map(|schema| &schema.relations)
            .map(|relation| relation.name.clone())
            .collect()
    })
}

#[gpui::test]
fn the_refresh_shortcut_finds_a_table_made_elsewhere_over_the_same_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        settle(cx);
        assert!(relation_names(&workspace, cx).is_empty());
        let generation = workspace.read_with(cx, |workspace, _| {
            workspace.profile().map(|profile| profile.generation)
        });

        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db");
        rusqlite::Connection::open(database)
            .expect("the database should open")
            .execute_batch("CREATE TABLE made_elsewhere (id INTEGER)")
            .expect("the table should be created");

        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        assert_eq!(relation_names(&workspace, cx), ["made_elsewhere"]);

        // Sent straight to the session, because its table has to stay
        // uncommitted: only a read beside the session cannot see it, and a
        // refresh that dropped the session would leave nothing to `COMMIT`.
        let session = workspace
            .read_with(cx, |workspace, _| {
                workspace.profile().and_then(|profile| profile.connection())
            })
            .expect("the profile is connected");
        let run = |sql: &str| {
            session
                .query(sql, &CancelToken::default(), Fetch::default())
                .expect("the statement should run");
        };
        run("BEGIN");
        run("CREATE TABLE not_committed (id INTEGER)");
        workspace.update(cx, |workspace, _| {
            if let Some(profile) = workspace.profile_mut() {
                profile.in_transaction = true;
            }
        });

        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        assert_eq!(relation_names(&workspace, cx), ["made_elsewhere"]);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the profile is still open");
            assert_eq!(Some(profile.generation), generation, "it reconnected");
            assert!(!profile.refreshing_schema());
            assert_eq!(
                profile.session.notice.as_deref(),
                Some("Schema refreshed."),
                "the read beside the session failed"
            );
        });

        run("COMMIT");
        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        let mut names = relation_names(&workspace, cx);
        names.sort();
        assert_eq!(names, ["made_elsewhere", "not_committed"]);

        quit(workspace, cx);
    });
}

fn history_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> crate::palette::HistoryView {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .palette
            .as_ref()
            .expect("the palette is up")
            .read(cx)
            .history()
    })
}

#[gpui::test]
fn the_history_scope_filters_and_sorts_its_runs_from_the_keyboard(cx: &mut TestAppContext) {
    use crate::palette::{HistoryOrder, HistoryStatus, HistoryView};
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let last = &workspace.profile().unwrap().session.history[0];
            assert_eq!((last.sql.as_str(), last.rows), ("select 1", Some(1)));
            assert!(last.at.is_some() && last.ms.is_some() && last.error.is_none());
        });

        // Outside History the chords do nothing.
        cx.simulate_keystrokes("secondary-p");
        settle(cx);
        cx.simulate_keystrokes("secondary-k f secondary-k o");
        settle(cx);
        assert_eq!(history_view(&workspace, cx), HistoryView::default());

        cx.simulate_keystrokes("tab tab");
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::Search(Scope::History)));
        cx.simulate_keystrokes("secondary-k f secondary-k o secondary-k o");
        settle(cx);
        assert_eq!(
            history_view(&workspace, cx),
            HistoryView {
                status: HistoryStatus::Succeeded,
                order: HistoryOrder::MostRows,
            }
        );

        // A palette opened again starts unfiltered.
        cx.simulate_keystrokes("escape");
        settle(cx);
        cx.simulate_keystrokes("secondary-p");
        settle(cx);
        assert_eq!(history_view(&workspace, cx), HistoryView::default());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_count_whose_tab_closed_under_it_still_leaves_its_outcome_in_the_history(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.executor().allow_parking();
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let object = workspace.read_with(cx, |workspace, _| {
            match workspace.profile().unwrap().session.active {
                crate::session::Tab::Object(id) => id,
                crate::session::Tab::Query(_) => panic!("the table did not open"),
            }
        });

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.count_rows(object, cx);
                workspace.close_object(object, window, cx);
            })
        });
        settle(cx);

        workspace.read_with(cx, |workspace, _| {
            let last = &workspace.profile().unwrap().session.history[0];
            assert!(last.sql.to_lowercase().contains("count"), "{}", last.sql);
            assert!(last.ms.is_some(), "the count's outcome was dropped");
        });
        let profile =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        assert!(store::history(&profile)[0].ms.is_some());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_new_folder_takes_its_name_from_the_keyboard_and_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf =
                    workspace.profile().unwrap().session.explorer_leaves["relation-main\0birds"];
                let member = workspace.member_in(&front_id(workspace), leaf.target);
                assert!(member.is_some(), "the table is nameable");
                workspace.new_folder(member, window, cx);
            });
        });
        settle(cx);
        let naming = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let session = &workspace.read(cx).profile().unwrap().session;
                session
                    .query_rename
                    .as_ref()
                    .map(|rename| rename.input.focus_handle(cx).is_focused(window))
            })
        };
        assert_eq!(
            naming(cx),
            Some(true),
            "the folder's field has the keyboard"
        );

        cx.simulate_input("Birds");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(naming(cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.update(|_, cx| {
            let profile = workspace.read(cx).profile().unwrap();
            let folders = &profile.shortcuts.folders;
            assert_eq!(folders.len(), 1);
            assert_eq!(folders[0].name, "Birds");
            assert_eq!(folders[0].members.len(), 1);
            let tree = profile.session.explorer_tree.read(cx);
            let row = tree.entry(0).expect("the folder leads the sidebar");
            assert_eq!(row.item().label.as_ref(), "Birds");
            assert_eq!(row.item().children[0].label.as_ref(), "birds");
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_folder_keeps_the_sidebar_drawn_while_the_catalog_is_away(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.update(|_, cx| {
            workspace.update(cx, |workspace, _| {
                let profile = workspace.profile_mut().unwrap();
                profile.catalog = CatalogState::Failed("gone".into());
                assert!(profile.session.saved_queries.is_empty());
                assert!(!super::forms::explorer_drawn(profile));

                profile.shortcuts.add_folder("Hot").unwrap();
                assert!(
                    super::forms::explorer_drawn(profile),
                    "its Remove and Unpin are reachable"
                );
            });
        });

        quit(workspace, cx);
    });
}

/// Where the sidebar draws `row`, by its id.
fn explorer_row(
    workspace: &Entity<Workspace>,
    row: &str,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    let index = cx
        .update(|_, cx| {
            let tree = workspace
                .read(cx)
                .profile()
                .unwrap()
                .session
                .explorer_tree
                .clone();
            tree.read(cx).index_of(&row.to_string().into())
        })
        .expect("the row is listed");
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("explorer-row-{index}").leak();
    cx.debug_bounds(selector).expect("the row is drawn")
}

/// The id of the row listing `member` in `folder`, or among the pins.
fn shortcut_row(
    workspace: &Entity<Workspace>,
    member: &explorer::Member,
    folder: Option<&str>,
    cx: &mut VisualTestContext,
) -> String {
    cx.update(|_, cx| {
        let session = &workspace.read(cx).profile().unwrap().session;
        let listed = explorer::Shortcut::Member {
            member: member.clone(),
            folder: folder.map(str::to_string),
            missing: false,
        };
        session
            .explorer_shortcuts
            .iter()
            .find(|(_, shortcut)| **shortcut == listed)
            .map(|(row, _)| row.clone())
            .expect("the shortcut is listed")
    })
}

/// A left-button drag from `from` to `to`, frame by frame, as the pointer
/// would make it.
fn drag(
    cx: &mut VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
    for at in [from + gpui::point(gpui::px(0.), gpui::px(8.)), to] {
        cx.simulate_mouse_move(at, gpui::MouseButton::Left, none);
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, none);
    settle(cx);
}

#[gpui::test]
fn a_shortcut_dragged_between_folders_moves_and_one_from_the_schemas_is_added(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let table = "relation-main\0birds";
        let birds = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves[table];
                let birds = workspace
                    .member_in(&front_id(workspace), leaf.target)
                    .unwrap();
                let shortcuts = &mut workspace.profile_mut().unwrap().shortcuts;
                shortcuts.add_folder("Hot").unwrap();
                shortcuts.add_folder("Cold").unwrap();
                workspace.toggle_in_folder("Hot".into(), birds.clone(), cx);
                birds
            })
        });
        settle(cx);
        let held = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let shortcuts = &workspace.read(cx).profile().unwrap().shortcuts;
                ["Hot", "Cold"].map(|name| shortcuts.holds(name, &birds))
            })
        };
        let in_hot = shortcut_row(&workspace, &birds, Some("Hot"), cx);

        let from = explorer_row(&workspace, &in_hot, cx).center();
        let to = explorer_row(&workspace, &explorer::folder_row("Cold"), cx).center();
        drag(cx, from, to);
        assert_eq!(
            held(cx),
            [false, true],
            "out of one folder into another moves"
        );

        let from = explorer_row(&workspace, table, cx).center();
        let to = explorer_row(&workspace, &explorer::folder_row("Hot"), cx).center();
        drag(cx, from, to);
        assert_eq!(held(cx), [true, true], "from the schemas it is added");
        assert!(
            sidebar_has_keyboard(&workspace, cx),
            "the sidebar keeps the keyboard"
        );
        assert!(
            cx.update(|_, cx| workspace
                .read(cx)
                .profile()
                .unwrap()
                .session
                .objects
                .is_empty()),
            "a drag opens nothing"
        );

        // Dropped where nothing takes it, a drag changes nothing.
        let from = explorer_row(&workspace, table, cx).center();
        drag(cx, from, from + gpui::point(gpui::px(0.), gpui::px(2_000.)));
        assert_eq!(held(cx), [true, true]);

        let at = explorer_row(&workspace, table, cx).center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(
            cx.update(|_, cx| workspace.read(cx).profile().unwrap().session.objects.len()),
            1,
            "a click still opens the row"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_pin_dropped_on_the_top_half_of_another_goes_before_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); create table fish (name text);",
        );
        let [birds, fish] = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                ["birds", "fish"].map(|name| {
                    let leaf = workspace.profile().unwrap().session.explorer_leaves
                        [&explorer::relation_row("main", name)];
                    let member = workspace
                        .member_in(&front_id(workspace), leaf.target)
                        .unwrap();
                    workspace.toggle_pin(explorer::Pin::Member(member.clone()), cx);
                    member
                })
            })
        });
        settle(cx);

        let from = explorer_row(&workspace, &shortcut_row(&workspace, &fish, None, cx), cx);
        let onto = explorer_row(&workspace, &shortcut_row(&workspace, &birds, None, cx), cx);
        drag(
            cx,
            from.center(),
            onto.origin + gpui::point(gpui::px(20.), gpui::px(2.)),
        );
        let pins = cx.update(|_, cx| workspace.read(cx).profile().unwrap().shortcuts.pins.clone());
        assert_eq!(
            pins,
            [explorer::Pin::Member(fish), explorer::Pin::Member(birds)]
        );

        quit(workspace, cx);
    });
}

/// Through the real restore and the real save: what `Profile::stored` writes
/// for a plugin profile is the user's string, never its password, and it
/// restores to the same connection.
#[gpui::test]
fn an_odbc_profile_saves_and_restores_as_it_was(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let odbc = store::tests::odbc_plugin();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        // A SQLite profile in front, since launching connects the active one
        // and a plugin profile would reach for the keychain.
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        let strings = [
            "Db2Dev",
            "DRIVER={PostgreSQL Unicode}; Server = pg.internal ;Port=5432;Opts={a;b}}c}",
        ];
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"dsn\"\nname = \"DSN\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"a@b.example\"\nengine = \"plugin:odbc\"\nspeaks = \"generic\"\n\
                 connection = {:?}\nstatement_timeout = 30\n\
                 [[profiles]]\nid = \"tunnel\"\nname = \"Tunnel\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"u\"\nengine = \"plugin:odbc\"\nspeaks = \"postgres\"\n\
                 connection = {:?}\n\
                 [profiles.ssh]\nhost = \"bastion.example\"\nport = 2222\nuser = \"me\"\n",
                database.display().to_string(),
                strings[0],
                strings[1],
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![odbc]);
        let external = |workspace: &Workspace| {
            workspace
                .profiles
                .iter()
                .filter(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .map(|profile| profile.config.clone())
                .collect::<Vec<_>>()
        };
        let restored = cx.update(|_, cx| external(workspace.read(cx)));
        let expected = |connection: &str, speaks, user: &str, timeout, ssh| {
            ConnectionConfig::External(ExternalConfig {
                plugin: odbc,
                speaks,
                connection: connection.into(),
                user: user.into(),
                password: String::new(),
                statement_timeout: timeout,
                ssh,
            })
        };
        assert_eq!(
            restored,
            [
                expected(strings[0], Engine::Generic, "a@b.example", 30, None),
                expected(
                    strings[1],
                    Engine::Postgres,
                    "u",
                    0,
                    Some(SshTunnel {
                        host: "bastion.example".into(),
                        port: Some(2222),
                        user: "me".into(),
                        identity_file: None,
                        run_as: None,
                    })
                ),
            ]
        );

        workspace.update(cx, |workspace, cx| {
            for profile in &mut workspace.profiles {
                if let ConnectionConfig::External(config) = &mut profile.config {
                    config.password = "hunter2".into();
                }
            }
            workspace.remember_profiles(cx);
        });
        let saved = std::fs::read_to_string(directory.join("profiles.toml")).unwrap();
        assert!(!saved.contains("hunter2"), "{saved}");
        let list = store::decode_profiles(&saved, &[odbc]).unwrap();
        let written = list
            .profiles
            .iter()
            .filter_map(|stored| {
                Some((
                    stored.engine.as_deref()?,
                    stored.speaks.as_deref()?,
                    stored.connection.as_deref()?,
                ))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            written,
            [
                ("plugin:odbc", "generic", strings[0]),
                ("plugin:odbc", "postgres", strings[1]),
            ]
        );

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.profiles.clear();
                for stored in list.profiles {
                    workspace.restore_profile(stored, window, cx);
                }
            });
        });
        assert_eq!(cx.update(|_, cx| external(workspace.read(cx))), restored);

        quit(workspace, cx);
    });
}

/// A duplicate has no Keychain entry of its own, so its Password field carries
/// the original's, for an ODBC profile as for a server.
#[gpui::test]
fn duplicating_an_odbc_profile_keeps_its_password(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"dsn\"\nname = \"DSN\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"a@b.example\"\nengine = \"plugin:odbc\"\nspeaks = \"generic\"\n\
                 connection = \"Db2Dev\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();
        let (workspace, cx) = launch_with(cx, vec![store::tests::odbc_plugin()]);
        let password = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let profile = workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == "dsn")
                    .unwrap();
                let form = ConnectionForm::duplicating(
                    profile,
                    "DSN copy".into(),
                    Some("hunter2".into()),
                    window,
                    cx,
                );
                form.password.read(cx).unmask_value().to_string()
            })
        });
        assert_eq!(password, "hunter2");
        quit(workspace, cx);
    });
}

/// What Save and Test read the form as refuses Run as beside a host that is
/// no socket directory, as a connect would, and leaves a blank one beside a
/// service to the connect, since the service may name a socket directory.
#[gpui::test]
fn the_forms_save_and_test_refuse_run_as_without_a_socket_directory(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        let postgres = |id: &str, host: &str, service: &str| {
            format!(
                "[[profiles]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{host}\"\n\
                 database = \"d\"\nuser = \"u\"\nengine = \"postgres\"\n{service}\
                 [profiles.ssh]\nhost = \"bastion\"\nrun_as = \"postgres\"\n"
            )
        };
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n{}{}{}",
                database.display().to_string(),
                postgres("tcp", "db.internal", ""),
                postgres("socket", "/var/run/postgresql", ""),
                postgres("service", "", "service = \"socketed\"\n"),
            ),
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        let read_as = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let profile = workspace
                        .profiles
                        .iter()
                        .find(|profile| profile.id == id)
                        .unwrap();
                    let form =
                        ConnectionForm::duplicating(profile, "Copy".into(), None, window, cx);
                    form.config(cx).map(|_| ())
                })
            })
        };
        let refused = read_as(cx, "tcp").unwrap_err();
        assert!(refused.starts_with("Run as needs Host"), "{refused}");
        assert_eq!(read_as(cx, "socket"), Ok(()));
        assert_eq!(read_as(cx, "service"), Ok(()));
        quit(workspace, cx);
    });
}

/// The ODBC tile is picked from the engine picker by keyboard, and the
/// keyboard goes on working through both field-set changes: the Connection
/// field takes it, and Host takes it back once Postgres unmounts that field.
#[gpui::test]
fn picking_the_odbc_tile_puts_the_keyboard_in_its_connection_string(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with(cx, vec![store::tests::odbc_plugin()]);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
            });
        });
        settle(cx);
        let has_keyboard =
            |cx: &mut VisualTestContext, field: fn(&ConnectionForm) -> &Entity<InputState>| {
                cx.update(|window, cx| {
                    let workspace = workspace.read(cx);
                    let form = workspace.form.as_ref().expect("the form is open");
                    field(form).focus_handle(cx).is_focused(window)
                })
            };
        let pick = |cx: &mut VisualTestContext, typed: &str| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_palette(PaletteMode::Engine, window, cx);
                });
            });
            settle(cx);
            cx.simulate_input(typed);
            cx.simulate_keystrokes("enter");
            settle(cx);
        };

        pick(cx, "odbc");
        assert!(palette_mode(&workspace, cx).is_none());
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            assert_eq!(form.target().fields(), crate::db::Fields::ConnectionString);
            assert_eq!(form.engine, Engine::Generic);
            assert_eq!(form.mode, crate::sql::Mode::ReadOnly);
        });
        assert!(has_keyboard(cx, |form| &form.connection));
        cx.simulate_input("Db2Dev");
        let typed = cx.update(|_, cx| {
            let workspace = workspace.read(cx);
            let form = workspace.form.as_ref().unwrap();
            form.connection.read(cx).value().to_string()
        });
        assert_eq!(typed, "Db2Dev");

        pick(cx, "postgres");
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            assert_eq!(form.target(), crate::db::Target::built_in(Engine::Postgres));
        });
        assert!(has_keyboard(cx, |form| &form.host));

        quit(workspace, cx);
    });
}

/// Read-only picked while a connect is under way finds no connection to hold
/// (`set_mode`), so the session that lands is opened again under it rather
/// than left as the Read-write it set out as, with Read-only on screen.
#[gpui::test]
fn a_mode_picked_while_connecting_is_the_one_the_session_holds(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let echo = crate::db::echo::plugin();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"postgres\"\n\
                 connection = \"DSN=echo\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![echo]);
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .expect("the echo profile is restored");
            assert_eq!(workspace.profiles[index].mode, crate::sql::Mode::ReadWrite);
            // Set so the connect never reaches for the keychain.
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx);
        });
        cx.run_until_parked();

        let connection = cx
            .update(|_, cx| {
                workspace
                    .read(cx)
                    .profile()
                    .and_then(|profile| profile.connection())
            })
            .expect("the echo profile connects");
        let last = connection
            .query("last", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(
            last.rows[0].get(0),
            Some("SET default_transaction_read_only = on")
        );

        quit(workspace, cx);
    });
}

/// A bar edit Generic refuses is put back, so the bars the tab saves stay the
/// ones its filter ran from.
#[gpui::test]
fn a_refused_bar_edit_leaves_the_bars_as_they_ran(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"generic\"\n\
                 connection = \"DSN=echo\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![crate::db::echo::plugin()]);
        // The connect and the tab's query run on threads the scheduler does
        // not own; echo answers both at once, so each wait below ends.
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .unwrap();
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
        });
        cx.run_until_parked();

        let bar = |column: &str, operator, value: &str| crate::filter::FilterBar {
            column: Some(column.into()),
            operator,
            value: value.into(),
            ..Default::default()
        };
        let ran = vec![
            bar("name", crate::filter::Operator::Contains, "ab"),
            bar("id", crate::filter::Operator::Equals, "50%"),
        ];
        let id = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let opened = crate::session::OpenedObject::Relation {
                    schema: "public".into(),
                    name: "accounts".into(),
                    kind: Default::default(),
                    filter: crate::filter::derived_filter(Engine::Generic, &ran, &[]),
                    filters: ran.clone(),
                };
                let id = workspace.open_object(opened, None, window, cx).unwrap();
                workspace.activate_tab(crate::session::Tab::Object(id), window, cx);
                id
            })
        });
        cx.run_until_parked();

        let saved = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let workspace = workspace.read(cx);
                let profile = workspace.profile().unwrap();
                let tab = profile
                    .session
                    .objects
                    .iter()
                    .find(|tab| tab.id == id)
                    .unwrap();
                let notice = profile.session.shown_notice().map(str::to_string);
                (tab.filters(Engine::Generic), notice)
            })
        };
        let as_ran: Vec<_> = ran.iter().map(crate::filter::stored_filter).collect();

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let input = workspace.filter_rows_mut(id).unwrap()[0].value.clone();
                input.update(cx, |input, cx| input.set_value("a%b", window, cx));
                workspace.submit_filter(id, &input, cx);
            })
        });
        let (bars, notice) = saved(cx);
        assert_eq!(bars, as_ran);
        assert!(notice.is_some_and(|notice| notice.contains("LIKE")));

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let action = crate::actions::SetFilterOperator {
                    row: 1,
                    operator: crate::filter::Operator::Contains,
                };
                workspace.set_filter_operator(&action, window, cx);
            })
        });
        assert_eq!(saved(cx).0, as_ran);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn pinning_leaves_every_row_its_height(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let table = explorer::relation_row("main", "birds");
        let unpinned = explorer_row(&workspace, &table, cx).size.height;

        cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves[&table];
                let birds = workspace
                    .member_in(&front_id(workspace), leaf.target)
                    .unwrap();
                workspace.toggle_pin(explorer::Pin::Member(birds), cx);
            })
        });
        settle(cx);

        assert_eq!(explorer_row(&workspace, &table, cx).size.height, unpinned);
        assert_eq!(
            explorer_row(&workspace, explorer::PINNED_GROUP, cx)
                .size
                .height,
            unpinned,
            "the Pinned group, listed first, is the row every row is sized by"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_service_launch_does_not_reuse_a_profiles_explicit_port(cx: &mut TestAppContext) {
    // PGSERVICE must be set before the process starts, not raced against
    // every other test that reads the environment.
    const CHILD: &str = "DBDELVE_SERVICE_PORT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "workspace::e2e::a_service_launch_does_not_reuse_a_profiles_explicit_port",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PGSERVICE", "startup-port")
            .env_remove("PGSERVICEFILE")
            .env_remove("PGSYSCONFDIR")
            .status()
            .expect("the child test should run");
        assert!(status.success());
        return;
    }

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "active = \"explicit\"\n\
             [settings]\n\
             check_for_updates = false\n\
             [[profiles]]\n\
             id = \"explicit\"\n\
             name = \"Explicit port\"\n\
             host = \"\"\n\
             port = 5432\n\
             database = \"\"\n\
             user = \"\"\n\
             service = \"startup-port\"\n\
             [[profiles]]\n\
             id = \"inherited\"\n\
             name = \"Service port\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             service = \"startup-port\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        let active =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        quit(workspace, cx);
        assert_eq!(active, "inherited");
    });
}

#[gpui::test]
fn an_upgraded_profile_file_loads_every_profile_hidden_and_saves_it_unchanged(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &format!(
                "{}{}",
                sqlite_profile("first", "First", ""),
                sqlite_profile("second", "Second", "")
            ),
        );
        assert!(!agent_visible(&workspace, cx, "first"));
        assert!(!agent_visible(&workspace, cx, "second"));
        quit(workspace, cx);

        let text =
            std::fs::read_to_string(store::dbdelve_directory().unwrap().join("profiles.toml"))
                .unwrap();
        assert!(!text.contains("agent_visible"), "{text}");
    });
}

#[gpui::test]
fn making_a_profile_visible_needs_the_prompt_and_survives_a_save(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &format!(
                "{}{}",
                sqlite_profile("first", "First", ""),
                sqlite_profile("second", "Second", "")
            ),
        );
        let set = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str, on| {
            workspace.update(cx, |workspace, cx| workspace.set_agent_visible(id, on, cx));
        };

        // No dialog open: asking changes nothing, in memory or on disk.
        set(&workspace, cx, "first", true);
        assert!(!agent_visible(&workspace, cx, "first"));
        assert!(!saved_visible("first"));

        // A dialog open for another profile grants nothing to this one, and
        // is not used up by the refusal.
        workspace.update(cx, |workspace, _| {
            workspace.agent_visibility_prompt = Some("second".into());
        });
        set(&workspace, cx, "first", true);
        assert!(!agent_visible(&workspace, cx, "first"));
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.agent_visibility_prompt.as_deref(), Some("second"));
        });

        // A prompt for a profile that does not exist grants nothing at all.
        set(&workspace, cx, "missing", true);
        assert!(!saved_visible("second"));

        // The prompt for this profile grants it, once, and the grant is saved.
        set(&workspace, cx, "second", true);
        assert!(agent_visible(&workspace, cx, "second"));
        assert!(saved_visible("second"));
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.agent_visibility_prompt, None);
        });
        set(&workspace, cx, "first", true);
        assert!(!agent_visible(&workspace, cx, "first"));

        // Hiding needs no prompt and lands at once.
        set(&workspace, cx, "second", false);
        assert!(!agent_visible(&workspace, cx, "second"));
        assert!(!saved_visible("second"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_mode_change_keeps_visibility_and_an_engine_change_hides(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("dev", "Dev", "agent_visible = true"));
        let index = workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "dev")
                .expect("the saved profile is restored");
            workspace.activate(index, cx);
            index
        });
        cx.run_until_parked();
        assert!(agent_visible(&workspace, cx, "dev"));

        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::Full, cx)
        });
        assert!(agent_visible(&workspace, cx, "dev"));

        let save = |workspace: &Entity<Workspace>,
                    cx: &mut VisualTestContext,
                    name: &str,
                    duckdb: bool| {
            workspace.update_in(cx, |workspace, window, cx| {
                let config = if duckdb {
                    crate::db::ConnectionConfig::duckdb(
                        store::dbdelve_directory()
                            .unwrap()
                            .join("never-created.duckdb")
                            .display()
                            .to_string(),
                        0,
                    )
                } else {
                    workspace.profiles[index].config.clone()
                };
                workspace.save_profile("dev", name.into(), config, None, window, cx);
            });
        };
        let notice = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .and_then(|profile| profile.session.notice.clone())
            })
        };

        // A new name on the same engine is not a reason to hide anything.
        save(&workspace, cx, "Renamed", false);
        assert!(agent_visible(&workspace, cx, "dev"));
        assert!(
            !notice(&workspace, cx)
                .unwrap_or_default()
                .starts_with("Renamed is hidden from agents")
        );
        assert!(saved_visible("dev"));

        // A new engine invalidates what the dialog stated, so it hides and says so.
        save(&workspace, cx, "Dev", true);
        assert!(!agent_visible(&workspace, cx, "dev"));
        assert_eq!(
            notice(&workspace, cx).as_deref(),
            Some("Dev is hidden from agents because its engine changed.")
        );
        assert!(!saved_visible("dev"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_profile_read_as_postgres_by_the_fallback_loads_hidden_and_is_saved_hidden(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        // `note__toml_private` sends the file through the plain decode, which
        // reads an engine this build cannot open as Postgres. The key is not a
        // field, so it is dropped and the launch save round-trips cleanly.
        let profiles = format!(
            "{}\
             [[profiles]]\nid = \"gone\"\nname = \"Gone\"\nhost = \"h\"\ndatabase = \"d\"\n\
             user = \"u\"\nengine = \"zz-unknown\"\nagent_visible = true\n\
             note__toml_private = 1\n\n\
             [[profiles]]\nid = \"odd\"\nname = \"Odd\"\nhost = \"h\"\ndatabase = \"d\"\n\
             user = \"u\"\nengine = \"generic\"\nagent_visible = true\n\n\
             {}",
            sqlite_profile("kept", "Kept", "agent_visible = true"),
            sqlite_profile("later", "Later", ""),
        );
        let (workspace, cx) = launch_with_profiles(cx, &profiles);

        for id in ["gone", "odd"] {
            workspace.read_with(cx, |workspace, _| {
                let profile = workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .expect("the profile was restored");
                assert_eq!(profile.config.engine(), Engine::Postgres, "{id} fell back");
                assert!(
                    profile
                        .session
                        .notice
                        .as_deref()
                        .is_some_and(|notice| notice.contains("Reading it as Postgres.")),
                    "{id} says it was read as Postgres"
                );
            });
        }

        assert!(
            !agent_visible(&workspace, cx, "gone"),
            "gone is hidden once read as Postgres"
        );
        assert!(!agent_visible(&workspace, cx, "odd"), "odd is hidden too");
        assert!(agent_visible(&workspace, cx, "kept"));
        assert!(!agent_visible(&workspace, cx, "later"));

        // Straight after launch, with no action: the hide is on disk, and the
        // profile after the narrowed ones was not lost to an early save.
        assert!(!saved_visible("gone"), "gone is saved hidden");
        assert!(!saved_visible("odd"), "odd is saved hidden");
        assert!(saved_visible("kept"), "a readable profile keeps its grant");
        let saved = store::peek_profiles(&[]).expect("profiles.toml reads back");
        for id in ["kept", "gone", "odd", "later"] {
            assert!(
                saved.profiles.iter().any(|profile| profile.id == id),
                "{id} survives the launch save"
            );
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.agent.visibility_choices.is_empty(),
                "the launch save leaves no pending choice"
            );
        });

        workspace.update(cx, |workspace, cx| workspace.remember_profiles(cx));
        assert!(!agent_visible(&workspace, cx, "gone"));
        assert!(!saved_visible("gone"), "a later save keeps the hide");

        // Another window grants it afterwards; this window adopts, not undoes.
        let mut list = store::peek_profiles(&[]).expect("profiles.toml reads back");
        list.profiles
            .iter_mut()
            .find(|profile| profile.id == "gone")
            .expect("the profile was saved")
            .agent_visible = true;
        store::save_profiles(&list).expect("the other window saves");
        workspace.update(cx, |workspace, cx| workspace.remember_profiles(cx));
        assert!(agent_visible(&workspace, cx, "gone"));
        assert!(saved_visible("gone"), "the other window's grant survives");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_unedited_save_keeps_visibility_and_a_new_host_hides(cx: &mut TestAppContext) {
    use super::agent::AgentLookup;

    store::tests::with_home(|| {
        // Snowflake has a host and no password slot, so neither the save nor the
        // reconnect reaches the keychain, and its connect fails on the missing
        // key before it reaches the network.
        let (workspace, cx) = launch_with_profiles(
            cx,
            "[[profiles]]\nid = \"warehouse\"\nname = \"Warehouse\"\n\
             host = \"dev.example.invalid\"\ndatabase = \"ANALYTICS\"\nuser = \"TIM\"\n\
             engine = \"snowflake\"\naccount = \"myorg-myaccount\"\n\
             private_key = \"/no-such-dir/dbdelve-e2e.p8\"\nagent_visible = true\n",
        );
        let index = workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "warehouse")
                .expect("the saved profile is restored");
            workspace.activate(index, cx);
            index
        });
        cx.run_until_parked();
        assert!(agent_visible(&workspace, cx, "warehouse"));
        let notice = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .and_then(|profile| profile.session.notice.clone())
            })
        };

        // The edit form submitted as it opened leaves the connection alone.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.edit_profile(index, window, cx);
            workspace.submit_connection(&SubmitConnection, window, cx);
        });
        workspace.read_with(cx, |workspace, _| assert!(workspace.form.is_none()));
        assert!(agent_visible(&workspace, cx, "warehouse"));
        assert!(saved_visible("warehouse"));
        assert!(
            !notice(&workspace, cx)
                .unwrap_or_default()
                .starts_with("Warehouse is hidden from agents")
        );

        // A new host alone points the consent at a server the owner did not name.
        workspace.update_in(cx, |workspace, window, cx| {
            let mut config = workspace.profiles[index].config.clone();
            if let ConnectionConfig::Snowflake(account) = &mut config {
                account.host = Some("prod.example.invalid".into());
            }
            workspace.save_profile("warehouse", "Warehouse".into(), config, None, window, cx);
        });
        assert!(!agent_visible(&workspace, cx, "warehouse"));
        assert!(!saved_visible("warehouse"));
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.agent_profile("Warehouse"),
                Err(AgentLookup::NotAvailable)
            ));
        });
        assert_eq!(
            notice(&workspace, cx).as_deref(),
            Some("Warehouse is hidden from agents because its connection changed.")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_hidden_profile_and_a_missing_one_answer_byte_for_byte_the_same(cx: &mut TestAppContext) {
    use super::agent::AgentLookup;

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("prod-hidden", "Prod", ""),
                sqlite_profile("prod", "Prod", visible),
                sqlite_profile("secret", "Secret", ""),
                sqlite_profile("staging", "Staging", visible),
                sqlite_profile("twin-1", "Twin", visible),
                sqlite_profile("twin-2", "Twin", visible),
                sqlite_profile("cafe", "Caf\u{e9}", visible),
                // An engine this build cannot open is held back from the
                // profile list, whatever it says about itself.
                "[[profiles]]\nid = \"legacy\"\nname = \"Legacy\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"plugin:odbc\"\nagent_visible = true\n"
                    .to_string(),
            ]
            .concat(),
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .profiles
                    .iter()
                    .all(|profile| profile.id != "legacy"),
                "a profile this build cannot open is not in the list"
            );

            let unanswered = [
                "Secret",
                "Missing",
                "secret",
                "Secre",
                "Secret ",
                "Legacy",
                "",
                "Cafe\u{301}",
            ]
            .map(|name| match workspace.agent_profile(name) {
                Err(AgentLookup::NotAvailable) => (
                    format!("{:?}", AgentLookup::NotAvailable),
                    AgentLookup::NotAvailable.to_string(),
                ),
                Err(other) => panic!("{name:?} answered {other:?}"),
                Ok(profile) => panic!("{name:?} resolved to {}", profile.id),
            });
            assert!(
                unanswered.iter().all(|answer| *answer == unanswered[0]),
                "{unanswered:?}"
            );

            assert_eq!(
                workspace.agent_profile("Prod").map(|p| p.id.as_str()),
                Ok("prod")
            );
            assert_eq!(
                workspace.agent_profile("Staging").map(|p| p.id.as_str()),
                Ok("staging")
            );
            assert_eq!(
                workspace.agent_profile("Caf\u{e9}").map(|p| p.id.as_str()),
                Ok("cafe")
            );

            let twin = workspace.agent_profile("Twin").map(|p| p.id.as_str());
            assert_eq!(twin, Err(AgentLookup::Ambiguous));
            assert_ne!(
                AgentLookup::Ambiguous.to_string(),
                AgentLookup::NotAvailable.to_string()
            );
            for lookup in [AgentLookup::NotAvailable, AgentLookup::Ambiguous] {
                let text = lookup.to_string();
                for name in ["Secret", "Missing", "Twin", "Prod", "Legacy"] {
                    assert!(!text.contains(name), "{text}");
                }
            }
        });
        quit(workspace, cx);
    });
}

fn run_command(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, command: Command) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.run_command(command, window, cx)
    });
    settle(cx);
}

fn visibility_prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| workspace.agent_visibility_prompt.clone())
}

#[gpui::test]
fn making_a_profile_visible_goes_through_the_dialog_that_opens_on_cancel(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(cx, &sqlite_profile("only", "Only", ""));

        run_command(&workspace, cx, Command::ShowToAgents(0));
        assert_eq!(
            front_dialog(&workspace, cx),
            Some("agent-visibility"),
            "{}",
            surface(&workspace, cx)
        );
        assert_eq!(ringed_button(&workspace, cx), 1);
        assert!(!agent_visible(&workspace, cx, "only"));

        // An `enter` carried over from the palette row lands on Cancel.
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(!agent_visible(&workspace, cx, "only"));
        assert!(!saved_visible("only"));

        run_command(&workspace, cx, Command::ShowToAgents(0));
        assert_eq!(ringed_button(&workspace, cx), 1);
        press(cx, "right");
        assert_eq!(ringed_button(&workspace, cx), 0);
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(visibility_prompt(&workspace, cx), None);
        assert!(agent_visible(&workspace, cx, "only"));
        assert!(saved_visible("only"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_visibility_dialog_names_the_commit_wait_only_for_a_rollback_journal_file(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(cx, &sqlite_profile("only", "Only", ""));
        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db");
        let blocks = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            run_command(workspace, cx, Command::ShowToAgents(0));
            let blocks =
                workspace.read_with(cx, |workspace, _| workspace.agent_visibility_blocks_commits);
            workspace.update(cx, |workspace, _| workspace.agent_visibility_prompt = None);
            blocks
        };

        rusqlite::Connection::open(&database)
            .and_then(|connection| connection.execute_batch("CREATE TABLE t (id INTEGER);"))
            .expect("the schema must apply");
        assert!(blocks(&workspace, cx));

        rusqlite::Connection::open(&database)
            .and_then(|connection| connection.execute_batch("PRAGMA journal_mode = WAL;"))
            .expect("the journal mode must change");
        assert!(!blocks(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn hiding_a_profile_takes_one_action_and_no_dialog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("only", "Only", "agent_visible = true"));
        assert!(agent_visible(&workspace, cx, "only"));

        run_command(&workspace, cx, Command::HideFromAgents(0));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(!agent_visible(&workspace, cx, "only"));
        assert!(!saved_visible("only"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn dismissing_the_visibility_dialog_grants_nothing_and_hands_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(cx, &sqlite_profile("only", "Only", ""));

        // `escape`, and `enter` on the Cancel the ring opens on.
        for (dismiss, typed) in [("escape", "x"), ("enter", "y")] {
            run_command(&workspace, cx, Command::ShowToAgents(0));
            assert_eq!(
                front_dialog(&workspace, cx),
                Some("agent-visibility"),
                "{}",
                surface(&workspace, cx)
            );
            let before = buffer(&workspace, cx);
            press(cx, dismiss);
            assert_eq!(front_dialog(&workspace, cx), None, "{dismiss} left it up");
            assert!(!agent_visible(&workspace, cx, "only"), "after {dismiss}");
            assert!(!saved_visible("only"), "after {dismiss}");
            assert_eq!(visibility_prompt(&workspace, cx), None, "after {dismiss}");
            assert!(editor_has_keyboard(&workspace, cx), "after {dismiss}");
            cx.simulate_input(typed);
            settle(cx);
            assert_eq!(buffer(&workspace, cx), format!("{before}{typed}"));
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_dialog_left_open_over_a_removed_profile_grants_nothing(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &format!(
                "{}{}",
                sqlite_profile("first", "First", ""),
                sqlite_profile("second", "Second", "")
            ),
        );
        run_command(&workspace, cx, Command::ShowToAgents(0));
        assert_eq!(
            front_dialog(&workspace, cx),
            Some("agent-visibility"),
            "{}",
            surface(&workspace, cx)
        );

        workspace.update(cx, |workspace, cx| workspace.remove_profile(0, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);

        workspace.update(cx, |workspace, cx| workspace.confirm_agent_visibility(cx));
        assert!(!agent_visible(&workspace, cx, "second"));
        assert!(!saved_visible("second"));
        assert_eq!(visibility_prompt(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// A palette row's text box clips what runs past it, so a marker laid out past
/// the box's right edge is drawn nowhere: a reachable connection then reads as
/// hidden. `debug_bounds` records the laid-out bounds even when an ancestor
/// clips, so the assertion is on where the marker sits, not whether it exists.
#[gpui::test]
fn a_visible_connection_keeps_its_marker_in_view_however_long_its_path_or_name(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let path = format!("{}/database.sqlite", "/a_long_folder_names_".repeat(9));
        assert!(path.len() >= 200);
        let long_name = "a_connection_named_at_great_length_".repeat(4);
        assert!(long_name.len() >= 120);
        let table = |id: &str, name: &str, path: &str| {
            format!(
                "[[profiles]]\nid = {id:?}\nname = {name:?}\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {path:?}\nagent_visible = true\n"
            )
        };
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                table("long-path", "Short", &path),
                table("long-name", &long_name, &path),
                table("control", "Control", "c.db"),
            ]
            .concat(),
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_connections(window, cx)
        });
        settle(cx);

        // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
        let bounds =
            |selector: String, cx: &mut VisualTestContext| cx.debug_bounds(selector.leak());
        let drawn: Vec<usize> = (0..32)
            .filter(|at| bounds(format!("palette-row-marker-{at}"), cx).is_some())
            .collect();
        let ids = ["long-path", "long-name", "control"];
        assert_eq!(
            drawn.len(),
            ids.len(),
            "a marker is laid out on each visible connection row, at {drawn:?}"
        );
        for (at, id) in drawn.into_iter().zip(ids) {
            let marker = bounds(format!("palette-row-marker-{at}"), cx).expect("counted above");
            let text = bounds(format!("palette-row-text-{at}"), cx)
                .unwrap_or_else(|| panic!("{id}: the row's text box is drawn"));
            assert!(
                text.left() <= marker.left() && marker.right() <= text.right(),
                "{id}: the marker spans {:?}..{:?}, outside its text box {:?}..{:?}",
                marker.left(),
                marker.right(),
                text.left(),
                text.right()
            );
            if id != "long-name" {
                let detail = bounds(format!("palette-row-detail-{at}"), cx)
                    .unwrap_or_else(|| panic!("{id}: the endpoint is drawn"));
                assert!(
                    detail.size.width > gpui::px(0.),
                    "{id}: the endpoint was squeezed to nothing"
                );
                assert!(
                    marker.right() <= detail.left(),
                    "{id}: the endpoint starts at {:?}, before the marker's end at {:?}",
                    detail.left(),
                    marker.right()
                );
            }
        }
        quit(workspace, cx);
    });
}

/// One call as a process on this machine makes it: connect to the socket, send
/// `frame` and a newline, read until the app closes the connection. On a thread
/// of its own, because the window answers only while the test drives it.
#[cfg(unix)]
fn raw_call(path: &std::path::Path, frame: &str) -> std::thread::JoinHandle<String> {
    use std::io::{Read, Write};

    let path = path.to_path_buf();
    let frame = format!("{frame}\n");
    std::thread::spawn(move || {
        let mut stream =
            std::os::unix::net::UnixStream::connect(&path).expect("the socket accepts a client");
        stream
            .write_all(frame.as_bytes())
            .expect("the frame is written");
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .expect("the app answers or closes");
        answer
    })
}

/// Let the window run while threads outside it work, until `done` holds. The
/// executor never advances its clock here, so only real time passes.
#[cfg(unix)]
fn drive_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    let started = std::time::Instant::now();
    while !done(cx) {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "nothing happened in five seconds"
        );
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[gpui::test]
fn an_agent_lists_the_visible_connections_by_name_and_engine_over_the_socket(
    cx: &mut TestAppContext,
) {
    use std::os::unix::fs::PermissionsExt;

    use crate::agent::{Reply, Status};
    use crate::session::ProfileState;

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("orders", "Orders", visible),
                sqlite_profile("billing", "Billing", visible),
                sqlite_profile("secret", "Secret", ""),
            ]
            .concat(),
        );
        cx.executor().allow_parking();
        let states = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profiles
                    .iter()
                    .map(|profile| matches!(profile.state, ProfileState::Idle))
                    .collect::<Vec<_>>()
            })
        };
        let before = states(cx);

        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        let client = raw_call(&path, &frame(r#""kind":"call","tool":"list_connections""#));
        drive_until(cx, |_| client.is_finished());
        let answer = client.join().expect("the client thread finished");

        let reply: Reply = serde_json::from_str(answer.trim_end()).expect("one answer frame");
        assert_eq!(reply.status, Status::Executed);
        let listed: serde_json::Value =
            serde_json::from_str(&reply.text).expect("the text is a JSON array");
        assert_eq!(
            listed,
            serde_json::json!([
                {"name": "Orders", "engine": "sqlite"},
                {"name": "Billing", "engine": "sqlite"},
            ])
        );
        for word in ["Secret", "path", ".db", "host", "user"] {
            assert!(!reply.text.contains(word), "{word}: {}", reply.text);
        }
        assert_eq!(states(cx), before, "listing opened or changed nothing");
        let mode = std::fs::metadata(path.parent().expect("the socket has a directory"))
            .expect("the directory exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        // Dropping the bridge stops the listener; the pump would otherwise
        // outlive the window.
        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_window_holds_a_presence_for_as_long_as_the_connection_is_open(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        let open = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.agent.presence.len())
        };

        let mut stream =
            std::os::unix::net::UnixStream::connect(&path).expect("the socket accepts a client");
        std::io::Write::write_all(&mut stream, format!("{}\n", presence()).as_bytes())
            .expect("the frame is written");
        drive_until(cx, |cx| open(cx) == 1);
        drop(stream);
        drive_until(cx, |cx| open(cx) == 0);

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

/// An agent's call arriving at the window as the socket would hand it over,
/// without a socket: no thread crosses the test scheduler. Answers the id it
/// was given and the end its answer will come out of.
fn inject_call(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    tool: crate::agent::tools::Tool,
    connection: Option<&str>,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    inject_call_with(
        workspace,
        cx,
        tool,
        connection,
        crate::agent::CallArgs::default(),
    )
}

/// [`inject_call`] for a call that carries arguments beyond its connection.
fn inject_call_with(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    tool: crate::agent::tools::Tool,
    connection: Option<&str>,
    args: crate::agent::CallArgs,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = crate::agent::CallId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let (reply, answers) = std::sync::mpsc::sync_channel(1);
    let call = crate::agent::Inbound::Call {
        id,
        tool,
        connection: connection.map(str::to_string),
        args,
        reply,
        gone: Default::default(),
    };
    workspace.update(cx, |workspace, cx| workspace.on_agent_message(call, cx));
    (id, answers)
}

/// An `execute_sql` call for `Orders` carrying `sql`, injected.
fn inject_sql(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection: &str,
    sql: &str,
) -> std::sync::mpsc::Receiver<crate::agent::Reply> {
    inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExecuteSql,
        Some(connection),
        crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            ..Default::default()
        },
    )
    .1
}

/// The app launched on a visible SQLite profile `Orders` (id `orders`) holding
/// a database `schema` has been run against, and a hidden one `Secret` on the
/// same file. `extra` is more scalar keys for `Orders`, such as its mode. The
/// profile is never connected: the file is written before anything opens it.
fn agent_sqlite<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let (workspace, cx) = launch_with_profiles(
        cx,
        // The app connects the first profile at launch, and `Orders` is to be
        // one the user never connected.
        &[
            sqlite_profile("secret", "Secret", ""),
            sqlite_profile(
                "orders",
                "Orders",
                &format!("agent_visible = true\n{extra}"),
            ),
        ]
        .concat(),
    );
    let database = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("e2e.db");
    rusqlite::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(schema))
        .expect("the schema must apply");
    (workspace, cx)
}

const ORDERS_SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT);\
     INSERT INTO t VALUES (1, 'a@x'), (2, NULL), (3, 'c@x');";

fn rows_in_orders() -> i64 {
    let database = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("e2e.db");
    rusqlite::Connection::open(database)
        .and_then(|connection| connection.query_row("SELECT count(*) FROM t", [], |row| row.get(0)))
        .expect("the table is readable")
}

fn pending_calls(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<crate::agent::CallId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.agent.calls.iter().map(|call| call.id).collect()
    })
}

/// An `execute_sql` call of [`UPDATE_ONE`] for `connection`, injected: what a
/// Read-write profile holds for a person. Unlike [`inject_approval`] it does
/// not say what happens to the call, so it serves a refusal and several waiting
/// calls as well.
fn inject_update(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection: Option<&str>,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExecuteSql,
        connection,
        crate::agent::CallArgs {
            sql: Some(UPDATE_ONE.to_string()),
            ..Default::default()
        },
    )
}

/// An `execute_sql` call the gate holds for approval, injected. The call is
/// pending when this returns.
fn inject_approval(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection: &str,
    sql: &str,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    let (id, answers) = inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExecuteSql,
        Some(connection),
        crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            ..Default::default()
        },
    );
    assert_eq!(pending_calls(workspace, cx), [id], "the call is held");
    (id, answers)
}

/// What a person does to approve the call in front: let the dialog be drawn,
/// wait out the arm, and click Approve.
fn approve_front(cx: &mut VisualTestContext) {
    settle(cx);
    cx.executor().advance_clock(super::agent::ARM_DELAY);
    settle(cx);
    let approve = cx
        .debug_bounds("agent-approval-approve")
        .expect("the dialog draws Approve");
    cx.simulate_click(approve.center(), gpui::Modifiers::default());
    settle(cx);
}

/// What `Orders`' file holds as the email of row `id`.
fn email_of(id: i64) -> Option<String> {
    let database = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("e2e.db");
    rusqlite::Connection::open(database)
        .and_then(|connection| {
            connection.query_row("SELECT email FROM t WHERE id = ?1", [id], |row| row.get(0))
        })
        .expect("the row is readable")
}

const UPDATE_ONE: &str = "UPDATE t SET email = 'z@x' WHERE id = 1";

/// The lines of the agent audit file as they were written. A file that does
/// not exist reads as no lines.
#[cfg(unix)]
fn audit_lines() -> Vec<serde_json::Value> {
    let path = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join(store::AGENT_AUDIT_FILE);
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).expect("a line of the audit is JSON"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[gpui::test]
fn an_approval_for_an_unknown_or_hidden_connection_is_refused_with_the_lookup_text(
    cx: &mut TestAppContext,
) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let visible = VISIBLE_RW;
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("orders", "Orders", visible),
                sqlite_profile("secret", "Secret", ""),
                sqlite_profile("dup-1", "Dup", visible),
                sqlite_profile("dup-2", "Dup", visible),
            ]
            .concat(),
        );
        let refused = |text: &str| Reply {
            status: Status::RefusedByGate,
            text: text.to_string(),
        };

        for name in [Some("Secret"), Some("Nope"), Some("orders"), None] {
            let (_, answers) = inject_update(&workspace, cx, name);
            assert_eq!(
                answers.try_recv().ok(),
                Some(refused("no agent-visible connection has that name")),
                "{name:?}"
            );
        }
        let (_, answers) = inject_update(&workspace, cx, Some("Dup"));
        assert_eq!(
            answers.try_recv().ok(),
            Some(refused(
                "more than one agent-visible connection has that name"
            ))
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_unanswered_approval_expires_after_the_fixed_wait(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use crate::agent::{EXPIRY, Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));

        cx.executor().advance_clock(EXPIRY - Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(
            answers.try_recv().ok(),
            Some(Reply {
                status: Status::Expired,
                text: "Nobody answered the approval in DBDelve within 5 minutes; the statement was not run."
                    .to_string(),
            })
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(answers.try_recv().is_err(), "nothing else is ever sent");
        assert_eq!(email_of(1).as_deref(), Some("a@x"), "the statement ran");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_approval_after_its_caller_hung_up_starts_nothing(cx: &mut TestAppContext) {
    use std::{io::Write, os::unix::net::UnixStream, time::Duration};

    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        let mut client = UnixStream::connect(&path).expect("the socket accepts a client");
        client
            .write_all(format!("{}\n", update_orders()).as_bytes())
            .expect("the frame is written");
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        let id = pending_calls(&workspace, cx)[0];
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.agent.armed),
            Some(id),
            "the approval is drawn and armed"
        );

        // The caller leaves and the window, not being run, does not hear it:
        // the person's click lands in the moment between the two.
        drop(client);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            pending_calls(&workspace, cx),
            [id],
            "the window has not heard"
        );

        workspace.update(cx, |workspace, cx| workspace.approve_call(id, cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty(), "a write was started");
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
        });
        let entries: Vec<_> = agent_log(&workspace, cx)
            .into_iter()
            .filter(|entry| entry.id == id)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, Status::Cancelled);
        assert!(entries[0].requester_gone);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        assert!(
            audit_lines()
                .iter()
                .all(|line| !(line["id"] == id.0 && line["phase"] == "started")),
            "nothing was recorded as started"
        );
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

#[cfg(unix)]
#[gpui::test]
fn hanging_up_withdraws_the_approval(cx: &mut TestAppContext) {
    use std::{
        io::{ErrorKind, Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        let held = format!("{}\n", update_orders());
        let held = held.as_bytes();
        let connect = || {
            let mut stream = UnixStream::connect(&path).expect("the socket accepts a client");
            stream.write_all(held).expect("the frame is written");
            stream
        };

        let mut first = connect();
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        let first_id = pending_calls(&workspace, cx)[0];
        let mut second = connect();
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 2);
        let second_id = pending_calls(&workspace, cx)[1];
        assert_ne!(first_id, second_id);

        // A caller that stops sending and waiting: it has hung up, and what it
        // is owed is no answer at all.
        first
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the timeout is set");
        first
            .shutdown(std::net::Shutdown::Write)
            .expect("the write half closes");
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        assert_eq!(pending_calls(&workspace, cx), [second_id]);
        let mut nothing = Vec::new();
        first
            .read_to_end(&mut nothing)
            .expect("the connection ends");
        assert!(nothing.is_empty(), "a withdrawn approval is sent nothing");
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        second
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("the timeout is set");
        let silent = second.read(&mut [0u8; 1]).expect_err("nothing was sent");
        assert!(matches!(
            silent.kind(),
            ErrorKind::WouldBlock | ErrorKind::TimedOut
        ));

        // Stopping the bridge leaves a pending call to the window, which
        // drops it: its caller sees the connection end with no answer.
        workspace.update(cx, |workspace, _| {
            workspace.agent.bridge.take();
            workspace.agent.calls.clear();
        });
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the timeout is set");
        let mut rest = Vec::new();
        second.read_to_end(&mut rest).expect("the connection ends");
        assert!(rest.is_empty());
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

/// What `agent_access` reads as in the file, which is what the next launch
/// would act on.
#[cfg(unix)]
fn saved_agent_access() -> Option<bool> {
    store::peek_profiles(&[])
        .expect("profiles.toml reads back")
        .settings
        .and_then(|settings| settings.agent_access)
}

/// The notice where `Workspace::note` put it: on the profile in front, else on
/// the connection form, else on the welcome surface.
fn shown_notice(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| match workspace.profile() {
        Some(profile) => profile.session.shown_notice().map(str::to_string),
        None => match &workspace.form {
            Some(form) => form.error.clone(),
            None => workspace.welcome_notice.clone(),
        },
    })
}

/// The labels of the palette's agent access row, which is one row or none: the
/// registration row is there whatever the access, and has its own test.
#[cfg(unix)]
fn agent_access_rows(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        crate::palette::agent_items(workspace)
            .into_iter()
            .map(|item| item.label)
            .filter(|label| label != "Copy Claude Code registration")
            .collect()
    })
}

#[cfg(unix)]
#[gpui::test]
fn copying_the_registration_puts_the_line_on_the_clipboard_and_says_so(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        assert!(workspace.read_with(cx, |workspace, _| {
            crate::palette::agent_items(workspace)
                .iter()
                .any(|item| item.label == "Copy Claude Code registration")
        }));

        run_command(&workspace, cx, Command::CopyAgentRegistration);
        let line = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("the line was copied");
        // The name and the option depend on the variant this process runs
        // under; the shape around them does not.
        assert!(line.starts_with("claude mcp add --scope user "), "{line}");
        assert!(line.ends_with("' mcp"), "{line}");
        assert!(line.contains(" -- '/"), "{line}");
        let notice = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .and_then(|profile| profile.session.notice.clone())
        });
        assert_eq!(
            notice.as_deref(),
            Some("Copied the Claude Code registration line.")
        );
    });
}

/// The frame of an `execute_sql` call for `Orders` that the gate holds for
/// approval on a Read-write profile.
#[cfg(unix)]
fn update_orders() -> String {
    frame(
        r#""kind":"call","tool":"execute_sql","connection":"Orders","sql":"UPDATE t SET email = 'z@x' WHERE id = 1""#,
    )
}

#[cfg(unix)]
#[gpui::test]
fn turning_agent_access_on_listens_and_turning_it_off_leaves_nothing_listening(
    cx: &mut TestAppContext,
) {
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};

    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        cx.executor().allow_parking();
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        assert_eq!(agent_access_rows(&workspace, cx), ["Turn agent access on"]);
        assert_eq!(saved_agent_access(), None);

        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        assert!(workspace.read_with(cx, |workspace, _| workspace.agent.bridge.is_some()));
        assert!(path.exists(), "the socket is bound at the derived path");
        let mode = std::fs::metadata(path.parent().expect("the socket has a directory"))
            .expect("the directory exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(saved_agent_access(), Some(true));
        assert_eq!(
            agent_access_rows(&workspace, cx),
            ["Disconnect agents and turn access off"]
        );

        // Another setting's save writes back what the file holds, not a copy
        // this window made at launch.
        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(false, cx));
        assert_eq!(saved_agent_access(), Some(true));

        let listed = raw_call(&path, &frame(r#""kind":"call","tool":"list_connections""#));
        drive_until(cx, |_| listed.is_finished());
        let reply: Reply = serde_json::from_str(
            listed
                .join()
                .expect("the client thread finished")
                .trim_end(),
        )
        .expect("one answer frame");
        assert_eq!(reply.status, Status::Executed);
        assert!(reply.text.contains("Orders"), "{}", reply.text);

        let held = raw_call(&path, &update_orders());
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);

        run_command(&workspace, cx, Command::DisconnectAgents);
        drive_until(cx, |_| held.is_finished());
        assert_eq!(
            held.join().expect("the client thread finished").trim_end(),
            r#"{"status":"rejected","text":"Agent access was turned off in DBDelve."}"#
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(workspace.read_with(cx, |workspace, _| workspace.agent.bridge.is_none()));
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "no file is left at the socket path"
        );
        assert!(UnixStream::connect(&path).is_err());
        assert_eq!(saved_agent_access(), Some(false));
        assert_eq!(agent_access_rows(&workspace, cx), ["Turn agent access on"]);

        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(true, cx));
        assert_eq!(saved_agent_access(), Some(false));

        // Pressed again with nothing listening, it changes nothing and says
        // nothing.
        workspace.update(cx, |workspace, _| workspace.clear_notice());
        run_command(&workspace, cx, Command::DisconnectAgents);
        assert_eq!(saved_agent_access(), Some(false));
        assert_eq!(shown_notice(&workspace, cx), None);

        quit(workspace, cx);
    });
}

/// Another window of the same variant, as far as the socket can tell: a
/// listener bound at the derived path, whose messages the test holds. Dropping
/// it is that window closing.
#[cfg(unix)]
fn another_window() -> (
    crate::agent::Listener,
    tokio::sync::mpsc::UnboundedReceiver<crate::agent::Inbound>,
) {
    let path = crate::agent::socket_path().expect("the test home gives a socket path");
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    match crate::agent::listen(&path, sender) {
        Ok(listener) => (listener, receiver),
        Err(_) => panic!("the stand-in window binds the socket"),
    }
}

/// What another window's own access action leaves in the shared settings file.
#[cfg(unix)]
fn another_window_saves_agent_access(choice: bool) {
    let mut list = store::peek_profiles(&[]).expect("profiles.toml reads back");
    list.settings
        .get_or_insert_with(Default::default)
        .agent_access = Some(choice);
    store::save_profiles(&list).expect("profiles.toml is written");
}

#[cfg(unix)]
const ALREADY_SERVED: &str = "Another DBDelve window is already serving agents.";

#[cfg(unix)]
const STILL_SERVED: &str = "Agent access was turned off. Another DBDelve window is still serving agents; they stay connected until that window closes or its own kill switch is used.";

#[cfg(unix)]
fn listening(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| workspace.agent.bridge.is_some())
}

#[cfg(unix)]
#[gpui::test]
fn a_saved_agent_access_listens_at_launch_and_quit_removes_the_socket(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let profile = sqlite_profile("orders", "Orders", "agent_visible = true");
        // The second `true` is the next launch from the file the first quit
        // left; the last two are a file that says off and one that says nothing.
        for (setting, saved, listens) in [
            ("agent_access = true", Some(true), true),
            ("agent_access = true", Some(true), true),
            ("agent_access = false", Some(false), false),
            ("", None, false),
        ] {
            let (workspace, cx) = launch_with_settings(cx, setting, &profile);
            cx.executor().allow_parking();
            assert_eq!(listening(&workspace, cx), listens, "{setting:?}");
            assert_eq!(path.exists(), listens, "{setting:?}");
            assert_eq!(
                agent_access_rows(&workspace, cx),
                [if listens {
                    "Disconnect agents and turn access off"
                } else {
                    "Turn agent access on"
                }]
            );

            quit(workspace, cx);
            assert!(
                std::fs::symlink_metadata(&path).is_err(),
                "{setting:?}: quitting leaves no socket file"
            );
            assert_eq!(saved_agent_access(), saved, "{setting:?}");
        }
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_second_window_leaves_a_live_socket_alone_and_says_so(cx: &mut TestAppContext) {
    use std::os::unix::net::UnixStream;

    store::tests::with_home(|| {
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let (first_window, _received) = another_window();
        let (workspace, cx) = launch_with_settings(
            cx,
            "agent_access = true",
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();

        assert!(!listening(&workspace, cx));
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(ALREADY_SERVED)
        );
        assert_eq!(saved_agent_access(), Some(true), "a decline writes nothing");
        assert!(
            UnixStream::connect(&path).is_ok(),
            "the first window still accepts"
        );

        workspace.update(cx, |workspace, _| workspace.clear_notice());
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        assert!(!listening(&workspace, cx));
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(ALREADY_SERVED)
        );
        assert_eq!(saved_agent_access(), Some(true));
        assert!(UnixStream::connect(&path).is_ok());

        drop(first_window);
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        assert!(listening(&workspace, cx));
        assert!(UnixStream::connect(&path).is_ok());
        assert_eq!(saved_agent_access(), Some(true));

        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_kill_switch_holds_across_two_windows_of_the_same_variant(cx: &mut TestAppContext) {
    use std::os::unix::net::UnixStream;

    store::tests::with_home(|| {
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let (first_window, mut received) = another_window();
        let (workspace, cx) = launch_with_settings(
            cx,
            "agent_access = true",
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        assert!(!listening(&workspace, cx));
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(ALREADY_SERVED)
        );

        // R-02: this window reaches none of the first window's agents, and
        // says so, while still saving that access is off for every launch.
        let press = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, _| workspace.clear_notice());
            run_command(&workspace, cx, Command::DisconnectAgents);
        };
        press(cx);
        assert_eq!(shown_notice(&workspace, cx).as_deref(), Some(STILL_SERVED));
        assert_eq!(saved_agent_access(), Some(false));
        assert!(
            UnixStream::connect(&path).is_ok(),
            "the first window is left serving"
        );
        assert!(
            received.try_recv().is_err(),
            "nothing was sent to the first window"
        );
        press(cx);
        assert_eq!(shown_notice(&workspace, cx).as_deref(), Some(STILL_SERVED));
        assert_eq!(saved_agent_access(), Some(false));

        // T-04-22: the other window's turn-on and kill switch each survive an
        // unrelated save made here.
        let toggle_titlebar = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let color = workspace.settings.color_titlebar;
                workspace.set_color_titlebar(!color, cx)
            });
        };
        another_window_saves_agent_access(true);
        toggle_titlebar(cx);
        assert_eq!(saved_agent_access(), Some(true));
        another_window_saves_agent_access(false);
        drop(first_window);
        toggle_titlebar(cx);
        assert_eq!(saved_agent_access(), Some(false));
        assert!(!listening(&workspace, cx));
        assert!(UnixStream::connect(&path).is_err());

        // The other window turned access on and quit: nothing serves, and
        // the kill saves off and has nothing to add.
        another_window_saves_agent_access(true);
        press(cx);
        assert_eq!(saved_agent_access(), Some(false));
        assert_eq!(shown_notice(&workspace, cx), None);
        let file = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("profiles.toml");
        let written = || {
            std::fs::metadata(&file)
                .and_then(|metadata| metadata.modified())
                .expect("profiles.toml has a modification time")
        };
        let before = written();
        std::thread::sleep(std::time::Duration::from_millis(20));
        press(cx);
        assert_eq!(written(), before, "nothing is written the second time");
        assert_eq!(shown_notice(&workspace, cx), None);

        quit(workspace, cx);
        let (workspace, cx) = launch(cx);
        assert!(!listening(&workspace, cx));
        assert!(std::fs::symlink_metadata(&path).is_err());
        quit(workspace, cx);
    });
}

#[cfg(unix)]
fn presence() -> String {
    frame(r#""kind":"presence""#)
}

#[cfg(unix)]
const REJECTED: &str = r#"{"status":"rejected","text":"Agent access was turned off in DBDelve."}"#;

#[cfg(unix)]
#[gpui::test]
fn the_kill_switch_rejects_every_pending_call_before_it_closes_the_socket(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");

        let held = [
            raw_call(&path, &update_orders()),
            raw_call(&path, &update_orders()),
        ];
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 2);
        let presence = raw_call(&path, &presence());
        drive_until(cx, |cx| {
            workspace.read_with(cx, |workspace, _| workspace.agent.presence.len() == 1)
        });
        // A waiting approval's dialog holds the keyboard, so the chord is pressed
        // from inside it, and has to reach the workspace from there.
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(dialog_has_keyboard(&workspace, cx));

        cx.simulate_keystrokes("secondary-shift-k");
        drive_until(cx, |_| {
            held.iter().all(|call| call.is_finished()) && presence.is_finished()
        });
        for call in held {
            // A whole frame, then the end of the connection: a bare end of
            // file would read as an empty string.
            assert_eq!(
                call.join().expect("the client thread finished"),
                format!("{REJECTED}\n")
            );
        }
        assert_eq!(
            presence.join().expect("the client thread finished"),
            "",
            "a presence is closed, and is sent nothing"
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.presence.is_empty());
            assert!(workspace.agent.bridge.is_none());
        });
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert_eq!(saved_agent_access(), Some(false));

        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_kill_switch_answers_its_chord_on_every_surface(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let path = crate::agent::socket_path().expect("the test home gives a socket path");

        // No profile: the welcome surface. A runner whose `PG*` variables make
        // a profile in front of it drives the main pane instead, which has
        // the same binding; the form is driven below either way.
        let (workspace, cx) = launch_with_settings(cx, "agent_access = true", "");
        cx.executor().allow_parking();
        assert!(listening(&workspace, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus.focus(window, cx)
        });
        settle(cx);
        cx.simulate_keystrokes("secondary-shift-k");
        settle(cx);
        assert!(!listening(&workspace, cx));
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert_eq!(saved_agent_access(), Some(false));
        quit(workspace, cx);

        // The connection form, opened over a profile.
        let (workspace, cx) = launch_with_settings(
            cx,
            "agent_access = true",
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        assert!(listening(&workspace, cx));
        run_command(&workspace, cx, Command::NewConnection);
        assert!(workspace.read_with(cx, |workspace, _| workspace.form.is_some()));
        cx.simulate_keystrokes("secondary-shift-k");
        settle(cx);
        assert!(!listening(&workspace, cx));
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert_eq!(saved_agent_access(), Some(false));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_click_on_an_empty_table_takes_the_keyboard_for_a_paste(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_on_sqlite_holding(cx, "create table nests (id integer primary key);");
        cx.executor().allow_parking();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("id\n5\n".into()));
        let nests = explorer_row(&workspace, "relation-main\0nests", cx).center();
        cx.simulate_click(nests, gpui::Modifiers::default());
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus.focus(window, cx)
        });
        settle(cx);

        let empty = cx
            .debug_bounds("empty-results")
            .expect("the message is drawn");
        cx.simulate_click(empty.center(), gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("secondary-v");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_ways_to_import_show_only_where_rows_can_be_imported(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key); insert into birds values (1);
             create table nests (id integer primary key);",
        );
        cx.executor().allow_parking();
        let drawn = |selector: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(selector).is_some()
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            let row: &'static str = format!("relation-main\0{name}").leak();
            let row = explorer_row(&workspace, row, cx).center();
            cx.simulate_click(row, gpui::Modifiers::default());
            settle(cx);
        };
        assert!(!drawn("import-rows", cx), "a query tab imports nothing");

        open("birds", cx);
        assert!(drawn("import-rows", cx));
        assert!(
            !drawn("paste-rows", cx),
            "a table with rows needs no invitation"
        );
        let menu_offers_paste = cx.update(|_, cx| {
            let session = &workspace.read(cx).profile().unwrap().session;
            session
                .active_results()
                .unwrap()
                .read(cx)
                .delegate()
                .takes_imports()
        });
        assert!(menu_offers_paste);

        open("nests", cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("id\n5\n".into()));
        let paste = cx
            .debug_bounds("paste-rows")
            .expect("an empty table invites rows");
        cx.simulate_click(paste.center(), gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);

        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        assert!(!drawn("paste-rows", cx), "read-only has nothing to invite");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn file_exports_from_the_menu_ask_for_the_right_file(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_on_sqlite_holding(cx, "create table birds (id integer primary key);");
        cx.executor().allow_parking();
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        for (downs, name) in [(3, "results.csv"), (4, "results.json"), (5, "results.sql")] {
            super::exports::SUGGESTED_NAMES.with_borrow_mut(Vec::clear);
            let bounds = cx
                .debug_bounds("export-results")
                .expect("Export button is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
            for _ in 0..downs {
                cx.simulate_keystrokes("down");
            }
            press(cx, "enter");
            cx.simulate_new_path_selection(|_| None);
            settle(cx);
            let asked = super::exports::SUGGESTED_NAMES.with_borrow(Clone::clone);
            assert_eq!(asked, [name], "{downs} downs");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_results_formats_work_from_menu_and_palette_and_restore_focus(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, note text);
             insert into birds values (1, null), (2, ''), (3, 'NULL');",
        );
        cx.executor().allow_parking();
        let sql = "select * from birds order by id";
        cx.simulate_input(sql);
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        let read_clipboard = |cx: &mut VisualTestContext| {
            let item = cx.read_from_clipboard().expect("copy writes the clipboard");
            let metadata = item.entries().iter().find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.clone(),
                _ => None,
            });
            (item.text().unwrap(), metadata)
        };
        let expected = |format: crate::export::ClipboardFormat, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let grid = workspace
                    .profile()
                    .unwrap()
                    .session
                    .active_results()
                    .unwrap()
                    .read(cx)
                    .delegate();
                format.render(Engine::Sqlite, grid.result()).unwrap()
            })
        };
        let open_menu = |cx: &mut VisualTestContext| {
            let bounds = cx
                .debug_bounds("export-results")
                .expect("Export button is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
            cx.update(|window, _| {
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu"))
                )
            });
        };

        for object_tab in [false, true] {
            if object_tab {
                let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
                cx.simulate_click(row, gpui::Modifiers::default());
                settle(cx);
            }
            open_menu(cx);
            cx.simulate_keystrokes("down");
            press(cx, "enter");
            assert_eq!(
                read_clipboard(cx).0,
                expected(crate::export::ClipboardFormat::Tsv, cx)
            );

            for (ix, format) in crate::export::ClipboardFormat::ALL.into_iter().enumerate() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
                open_menu(cx);
                cx.simulate_keystrokes("down down");
                settle(cx);
                // The menu hangs off the right edge, so its submenu opens leftwards.
                cx.simulate_keystrokes("left");
                settle(cx);
                for _ in 0..ix {
                    cx.simulate_keystrokes("down");
                }
                press(cx, "enter");
                let (text, metadata) = read_clipboard(cx);
                assert_eq!(text, expected(format, cx), "menu {format:?}");
                let metadata = metadata.expect("all successful table copies carry original rows");
                let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
                assert_eq!(metadata["rows"][0]["note"], serde_json::Value::Null);
                assert_eq!(metadata["rows"][1]["note"], "");
                assert_eq!(metadata["rows"][2]["note"], "NULL");
                assert!(metadata["columns"][0]["type"].is_string());
                let (_, bytes, _) = crate::import_rows::pasted(
                    &text,
                    Some(&serde_json::to_string(&metadata).unwrap()),
                    false,
                )
                .unwrap();
                let pasted: Vec<serde_json::Value> = String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(pasted, metadata["rows"].as_array().unwrap().clone());
                settle(cx);
                cx.update(|window, _| {
                    assert!(
                        !window
                            .context_stack()
                            .iter()
                            .any(|context| context.contains("PopupMenu"))
                    )
                });

                cx.write_to_clipboard(gpui::ClipboardItem::new_string("before palette".into()));
                cx.simulate_keystrokes("secondary-shift-p");
                settle(cx);
                cx.simulate_input(&format!("Copy results as {}", format.label()));
                press(cx, "enter");
                assert_eq!(read_clipboard(cx).0, text, "palette {format:?}");
            }
            open_menu(cx);
            press(cx, "escape");
            cx.update(|window, cx| {
                assert!(
                    !window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu"))
                );
                assert!(
                    window.focused(cx).is_some(),
                    "Escape returns a live dispatch path"
                );
            });
            // The existing copy action still defaults to TSV after a table format was used.
            cx.simulate_keystrokes("secondary-k r");
            settle(cx);
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(crate::actions::CopyResults), cx);
            });
            assert_eq!(
                read_clipboard(cx).0,
                expected(crate::export::ClipboardFormat::Tsv, cx)
            );
            if !object_tab {
                assert_eq!(buffer(&workspace, cx), sql);
            } else {
                let grid = workspace.read_with(cx, |workspace, _| {
                    workspace
                        .profile()
                        .unwrap()
                        .session
                        .active_results()
                        .unwrap()
                        .clone()
                });
                grid.update(cx, |grid, cx| {
                    assert!(grid.delegate_mut().set_pending(
                        0,
                        1,
                        crate::result_grid::NewValue::Value("pending".into())
                    ));
                    cx.notify();
                });
                workspace.update(cx, |_, cx| cx.notify());
                settle(cx);
                assert!(cx.debug_bounds("export-results").is_none());
                for format in crate::export::ClipboardFormat::ALL {
                    workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
                    assert_eq!(read_clipboard(cx).0, expected(format, cx));
                    assert!(!read_clipboard(cx).0.contains("pending"));
                }
                grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
            }
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn refused_table_copies_preserve_the_clipboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let grid = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .unwrap()
                .session
                .active_results()
                .unwrap()
                .clone()
        });
        let column = crate::db::Column {
            name: "v".into(),
            data_type: Some("text".into()),
        };
        let result = crate::db::QueryResult {
            columns: vec![column.clone()],
            rows: crate::db::rows(vec![vec![Some(
                "x".repeat(crate::result_grid::COPIED_ROWS_TEXT_LIMIT),
            )]]),
            ..Default::default()
        };
        grid.update(cx, |grid, _| {
            *grid.delegate_mut() =
                crate::result_grid::ResultGrid::new(result, crate::sql::Mode::ReadOnly);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("previous clipboard".into()));
        for format in crate::export::ClipboardFormat::ALL.into_iter().skip(2) {
            workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("previous clipboard")
            );
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.profile().unwrap().session.notice.as_deref(),
                    Some("Table output exceeds 16 MiB. Copy as TSV/CSV or export to a file.")
                );
            });
        }
        let width = (crate::result_grid::COPIED_ROWS_TEXT_LIMIT - 24) / 5;
        let value = format!("{}\u{301}", "x".repeat(width));
        grid.update(cx, |grid, _| {
            *grid.delegate_mut() = crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column.clone()],
                    rows: crate::db::rows(vec![vec![Some(value.clone())]]),
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            );
        });
        workspace.update(cx, |workspace, cx| {
            workspace.copy_results_as(crate::export::ClipboardFormat::AsciiTable, cx);
        });
        let item = cx.read_from_clipboard().unwrap();
        assert_eq!(
            item.text().unwrap().len(),
            crate::result_grid::COPIED_ROWS_TEXT_LIMIT
        );
        let metadata = item
            .entries()
            .iter()
            .find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.as_deref(),
                _ => None,
            })
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(metadata["rows"][0]["v"], value);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("previous clipboard".into()));
        for result in [
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult::default(),
                crate::sql::Mode::ReadOnly,
            ),
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column.clone()],
                    capped_from: Some(10),
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            ),
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column],
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            )
            .streaming(0),
        ] {
            grid.update(cx, |grid, _| *grid.delegate_mut() = result);
            for format in crate::export::ClipboardFormat::ALL {
                workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().as_deref(),
                    Some("previous clipboard")
                );
            }
        }
        quit(workspace, cx);
    });
}

/// The call the approval dialog is about.
fn front_approval_id(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::agent::CallId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.front_approval().map(|call| call.id)
    })
}

fn denied() -> crate::agent::Reply {
    crate::agent::Reply {
        status: crate::agent::Status::Rejected,
        text: "The statement was denied in DBDelve and was not run.".to_string(),
    }
}

#[cfg(unix)]
#[gpui::test]
fn an_approved_statement_waits_for_the_human_and_runs_only_after_approve(cx: &mut TestAppContext) {
    use crate::{agent::Status, session::ProfileState, sql::Mode};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");

        let shim = crate::agent::shim::testing::call_through_the_shim(
            path,
            "execute_sql",
            serde_json::json!({ "connection": "Orders", "sql": UPDATE_ONE }),
        );
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(!shim.is_finished(), "the agent waits for the human");
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        assert_eq!(email_of(1).as_deref(), Some("a@x"));

        approve_front(cx);
        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(
            result["content"][0]["text"],
            "status: executed\nThe server reported 1 row affected."
        );
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty());
            assert!(workspace.agent.lanes.is_empty());
            let orders = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            assert!(
                matches!(orders.state, ProfileState::Idle),
                "approving a write connected the user's profile"
            );
            let entry = workspace.agent.log.front().expect("the call is logged");
            assert_eq!(entry.status, Status::Executed);
            assert_eq!(
                entry.verdict,
                Some(super::agent::AuditVerdict::Approve {
                    mode: Mode::ReadWrite,
                    expiry: None
                })
            );
        });
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approved_ddl_says_it_ran_not_that_no_row_changed(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        for (sql, text) in [
            ("CREATE INDEX i ON t (email)", "The statement ran."),
            (
                "UPDATE t SET email = 'q' WHERE id > 1",
                "The server reported 2 rows affected.",
            ),
        ] {
            let (_, answers) = inject_approval(&workspace, cx, "Orders", sql);
            approve_front(cx);
            let answer = answer_of(cx, &answers);
            assert_eq!(answer.status, Status::Executed, "{sql}: {}", answer.text);
            assert_eq!(answer.text, text, "{sql}");
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_approved_write_that_fails_answers_and_records_the_same_words(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", "UPDATE missing SET a = 1");
        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Failed, "{}", answer.text);
        assert!(!answer.text.is_empty());

        let lines = audit_of(id);
        let outcome = lines.last().expect("the call was audited");
        assert_eq!(outcome["phase"], "outcome");
        assert_eq!(outcome["status"], "failed");
        assert_eq!(outcome["error"], answer.text.as_str(), "{outcome}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn approve_is_inert_until_armed(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use super::agent::ARM_DELAY;
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        let state = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (workspace.agent.revealed, workspace.agent.armed)
            })
        };
        assert_eq!(state(cx), (Some(id), None), "the dialog was revealed");

        let approve = cx
            .debug_bounds("agent-approval-approve")
            .expect("the dialog draws Approve");
        cx.simulate_click(approve.center(), gpui::Modifiers::default());
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.approve_call(id, cx));
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(email_of(1).as_deref(), Some("a@x"));

        cx.executor()
            .advance_clock(ARM_DELAY - Duration::from_millis(1));
        settle(cx);
        assert_eq!(state(cx).1, None, "armed a millisecond early");
        cx.executor().advance_clock(Duration::from_millis(1));
        settle(cx);
        assert_eq!(state(cx).1, Some(id));

        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approval_covered_by_another_dialog_is_not_armed_until_it_is_shown_again(
    cx: &mut TestAppContext,
) {
    use std::time::Duration;

    use super::agent::ARM_DELAY;
    use crate::actions::ShowAgentActivity;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, _answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        let armed = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.agent.armed);

        cx.update(|window, cx| window.dispatch_action(Box::new(ShowAgentActivity), cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-audit"));
        cx.executor()
            .advance_clock(ARM_DELAY + Duration::from_millis(1));
        settle(cx);
        assert_eq!(
            armed(cx),
            None,
            "armed under a dialog the person is reading"
        );

        workspace.update(cx, |workspace, cx| workspace.close_agent_audit(cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(armed(cx), None, "armed the moment it was uncovered");
        cx.executor()
            .advance_clock(ARM_DELAY - Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), None);
        cx.executor().advance_clock(Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), Some(id));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approval_under_the_palette_is_not_armed_until_the_palette_closes(cx: &mut TestAppContext) {
    use std::time::Duration;

    use super::agent::ARM_DELAY;
    use crate::actions::CommandPalette;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let armed = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.agent.armed);
        let open_palette = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.dispatch_action(Box::new(CommandPalette), cx));
            settle(cx);
            assert!(workspace.read_with(cx, |w, _| w.palette.is_some()));
        };
        let close_palette = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.close_palette(window, cx)
            });
            settle(cx);
        };

        // A call that arrives under the palette.
        open_palette(cx);
        let (id, _answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        assert_eq!(
            workspace.read_with(cx, |w, _| w.agent.attended),
            Some(id),
            "the window asks for attention all the same"
        );
        cx.executor()
            .advance_clock(ARM_DELAY + Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), None, "armed under the palette");

        close_palette(cx);
        assert_eq!(armed(cx), None, "armed the moment the palette closed");
        cx.executor()
            .advance_clock(ARM_DELAY - Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), None);
        cx.executor().advance_clock(Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), Some(id));

        // The palette opened over an armed approval disarms it until it closes.
        open_palette(cx);
        assert_eq!(armed(cx), None, "still armed under the palette");
        close_palette(cx);
        assert_eq!(armed(cx), None);
        cx.executor().advance_clock(ARM_DELAY);
        settle(cx);
        assert_eq!(armed(cx), Some(id));
        quit(workspace, cx);
    });
}

/// A press and release of the left button at `at`, with the counts a person's
/// double click or a window-activating click carries.
fn mouse_press(
    cx: &mut VisualTestContext,
    at: gpui::Point<gpui::Pixels>,
    count: usize,
    first: bool,
) {
    cx.simulate_event(gpui::MouseDownEvent {
        button: gpui::MouseButton::Left,
        position: at,
        modifiers: gpui::Modifiers::default(),
        click_count: count,
        first_mouse: first,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: gpui::MouseButton::Left,
        position: at,
        modifiers: gpui::Modifiers::default(),
        click_count: count,
    });
    settle(cx);
}

/// Approve's centre, once it is drawn.
fn approve_centre(cx: &mut VisualTestContext) -> gpui::Point<gpui::Pixels> {
    cx.debug_bounds("agent-approval-approve")
        .expect("the dialog draws Approve")
        .center()
}

#[gpui::test]
fn a_double_or_activating_click_never_approves(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        assert_eq!(workspace.read_with(cx, |w, _| w.agent.armed), Some(id));

        let at = approve_centre(cx);
        for (count, first) in [(2, false), (1, true)] {
            mouse_press(cx, at, count, first);
            assert_eq!(pending_calls(&workspace, cx), [id], "{count} {first}");
            assert_eq!(
                answers.try_recv().err(),
                Some(TryRecvError::Empty),
                "{count} {first}"
            );
            assert_eq!(email_of(1).as_deref(), Some("a@x"), "{count} {first}");
        }

        mouse_press(cx, at, 1, false);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_revealed_approval_arms_again(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use super::agent::ARM_DELAY;
    use crate::{actions::ShowAgentActivity, agent::Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(ARM_DELAY);
        settle(cx);
        let armed = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.agent.armed);
        assert_eq!(armed(cx), Some(id), "armed the first time");

        cx.update(|window, cx| window.dispatch_action(Box::new(ShowAgentActivity), cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-audit"));
        workspace.update(cx, |workspace, cx| workspace.close_agent_audit(cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(armed(cx), None, "armed the moment it was uncovered");

        let at = approve_centre(cx);
        mouse_press(cx, at, 1, false);
        assert_eq!(pending_calls(&workspace, cx), [id], "approved while inert");
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(email_of(1).as_deref(), Some("a@x"));

        cx.executor()
            .advance_clock(ARM_DELAY - Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), None, "armed a millisecond early");
        cx.executor().advance_clock(Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), Some(id));

        let at = approve_centre(cx);
        mouse_press(cx, at, 1, false);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_next_approval_is_inert_after_the_first_is_answered(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use super::agent::ARM_DELAY;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (first, first_answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        let (second, second_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some(UPDATE_ONE.to_string()),
                ..Default::default()
            },
        );
        settle(cx);
        cx.executor().advance_clock(ARM_DELAY);
        settle(cx);
        let armed = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.agent.armed);
        assert_eq!(armed(cx), Some(first));

        workspace.update(cx, |workspace, cx| workspace.deny_call(first, cx));
        settle(cx);
        assert_eq!(first_answers.try_recv().ok(), Some(denied()));
        assert_eq!(pending_calls(&workspace, cx), [second]);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(armed(cx), None, "the next call inherited the arm");

        let at = approve_centre(cx);
        mouse_press(cx, at, 1, false);
        assert_eq!(pending_calls(&workspace, cx), [second]);
        assert_eq!(second_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(email_of(1).as_deref(), Some("a@x"));

        cx.executor()
            .advance_clock(ARM_DELAY - Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), None, "armed a millisecond early");
        cx.executor().advance_clock(Duration::from_millis(1));
        settle(cx);
        assert_eq!(armed(cx), Some(second));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn approving_nothing_changes_nothing(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let shape = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, _| {
                (w.agent.calls.len(), w.agent.log.len(), w.agent.writes.len())
            })
        };
        let before = shape(cx);
        workspace.update(cx, |workspace, cx| {
            assert!(
                !workspace.deny_front_approval(cx),
                "escape spent on nothing"
            );
            workspace.approve_call(crate::agent::CallId(u64::MAX), cx);
        });
        assert_eq!(shape(cx), before, "nothing pending");

        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.approve_call(crate::agent::CallId(u64::MAX), cx)
        });
        assert_eq!(pending_calls(&workspace, cx), [id], "an id never issued");
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));

        workspace.update(cx, |workspace, cx| workspace.deny_call(id, cx));
        settle(cx);
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        let denied_shape = shape(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.approve_call(id, cx);
            assert!(!workspace.deny_front_approval(cx));
        });
        assert_eq!(shape(cx), denied_shape, "a call already denied");
        // Its reply was sent once and the call dropped with it.
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Disconnected));
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn attention_is_asked_once_per_approval(cx: &mut TestAppContext) {
    use crate::actions::ShowAgentActivity;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let attended =
            |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.agent.attended);
        settle(cx);
        assert_eq!(attended(cx), None, "asked with nothing to approve");

        let (first, _first_answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        assert_eq!(attended(cx), Some(first));

        // Covered and revealed again: the same call, so no second request.
        cx.update(|window, cx| window.dispatch_action(Box::new(ShowAgentActivity), cx));
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.close_agent_audit(cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(
            workspace.read_with(cx, |w, _| w.agent.revealed),
            Some(first)
        );
        assert_eq!(attended(cx), Some(first));

        let (second, _second_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some(UPDATE_ONE.to_string()),
                ..Default::default()
            },
        );
        settle(cx);
        assert_eq!(
            attended(cx),
            Some(first),
            "asked for a call behind the front"
        );

        workspace.update(cx, |workspace, cx| workspace.deny_call(first, cx));
        settle(cx);
        assert_eq!(attended(cx), Some(second));

        workspace.update(cx, |workspace, cx| workspace.deny_call(second, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(attended(cx), Some(second), "asked with nothing to approve");
        quit(workspace, cx);
    });
}

#[test]
fn the_approval_surface_animates_nothing() {
    let forms = include_str!("forms.rs");
    let from = forms
        .find("fn render_agent_approval")
        .expect("the approval render exists");
    let render = &forms[from..];
    let render = &render[..render
        .find("\n    pub(crate) fn ")
        .expect("another method follows the render")];
    for moving in [
        "with_animation",
        "Animation::",
        "smooth_scroll",
        "Spinner",
        "request_animation_frame",
    ] {
        assert!(
            !render.contains(moving),
            "the approval surface holds {moving}"
        );
    }

    let agent = include_str!("agent.rs");
    let from = agent.find("fn reveal_approval").expect("the reveal exists");
    let reveal = &agent[from..];
    let reveal = &reveal[..reveal
        .find("\n    pub(crate) fn ")
        .expect("another method follows the reveal")];
    assert_eq!(reveal.matches("request_attention").count(), 1);
    assert!(
        !reveal.contains("activate"),
        "the reveal takes the window forward"
    );
    for (file, text) in [
        ("forms.rs", forms),
        ("mod.rs", include_str!("mod.rs")),
        ("tabs.rs", include_str!("tabs.rs")),
    ] {
        assert!(
            !text.contains("request_attention"),
            "{file} asks for attention"
        );
    }
}

#[gpui::test]
fn the_approval_draws_every_line_of_a_long_statement(cx: &mut TestAppContext) {
    use crate::sql::{AgentVerdict, agent_gate};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        // The carriage return after the last token, not inside a comment,
        // which the gate may refuse as unreadable.
        let comments = (0..299)
            .map(|n| format!("-- line {n}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let sql = format!("{comments}\r\n{UPDATE_ONE}\r");
        assert!(
            matches!(
                agent_gate(Engine::Sqlite, &sql, crate::sql::Mode::ReadWrite),
                AgentVerdict::Approve(..)
            ),
            "the statement is one the gate holds for approval"
        );
        inject_approval(&workspace, cx, "Orders", &sql);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));

        let block = cx
            .debug_bounds("agent-approval-sql")
            .expect("the statement block is drawn");
        assert!(
            f32::from(block.size.height) <= theme::layout::APPROVAL_TEXT_MAX_HEIGHT,
            "the block is {:?} tall",
            block.size.height
        );
        for line in [0, 299] {
            assert!(
                cx.debug_bounds(format!("agent-approval-line-{line}").leak())
                    .is_some(),
                "line {line} has no element"
            );
        }
        assert!(cx.debug_bounds("agent-approval-label").is_some());
        assert!(cx.debug_bounds("agent-approval-size").is_some());
        let window = cx.update(|window, _| window.viewport_size());
        let window =
            gpui::Bounds::from_corners(gpui::point(gpui::px(0.), gpui::px(0.)), window.into());
        for button in ["agent-approval-0", "agent-approval-approve"] {
            let bounds = cx.debug_bounds(button).expect("the button is drawn");
            assert!(
                window.contains(&bounds.origin) && window.contains(&bounds.bottom_right()),
                "{button} at {bounds:?} is outside the window {window:?}"
            );
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn on_a_short_window_the_approval_keeps_deny_and_approve_on_screen(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // A file not in WAL mode, so the write carries its lock fact too.
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let comments = (0..299)
            .map(|n| format!("-- line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        inject_approval(
            &workspace,
            cx,
            "Orders",
            &format!("{comments}\n{UPDATE_ONE}"),
        );
        // A second call, so "1 more waiting" is drawn as well.
        inject_update(&workspace, cx, Some("Orders"));
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(600.)));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(cx.debug_bounds("agent-approval-waiting").is_some());

        let window = cx.update(|window, _| window.viewport_size());
        assert_eq!(f32::from(window.height), 600.);
        let window =
            gpui::Bounds::from_corners(gpui::point(gpui::px(0.), gpui::px(0.)), window.into());
        for button in ["agent-approval-0", "agent-approval-approve"] {
            let bounds = cx.debug_bounds(button).expect("the button is drawn");
            assert!(
                window.contains(&bounds.origin) && window.contains(&bounds.bottom_right()),
                "{button} at {bounds:?} is outside the window {window:?}"
            );
        }
        let block = cx
            .debug_bounds("agent-approval-sql")
            .expect("the statement block is drawn");
        assert!(
            f32::from(block.size.height) > 40.,
            "the statement is squeezed to {:?}",
            block.size.height
        );
        let bar = cx
            .debug_bounds("agent-approval-scrollbar")
            .expect("the statement block draws a scrollbar");
        assert_eq!(bar, block, "the scrollbar lies over the statement block");

        // The scrollbar over it does not take the wheel from the statement.
        let offset = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, _| {
                w.approval_scroll
                    .borrow()
                    .as_ref()
                    .map(|(_, handle)| handle.offset().y)
            })
        };
        assert_eq!(offset(cx), Some(gpui::px(0.)));
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: block.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-120.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(cx);
        assert!(
            offset(cx).is_some_and(|y| y < gpui::px(0.)),
            "the wheel did not scroll the statement: {:?}",
            offset(cx)
        );
        quit(workspace, cx);
    });
}

#[test]
fn the_approval_draws_its_own_statement_not_the_audit_copy() {
    let forms = include_str!("forms.rs");
    let from = forms
        .find("fn render_agent_approval")
        .expect("the approval render exists");
    let render = &forms[from..];
    let render = &render[..render
        .find("\n    pub(crate) fn ")
        .expect("another method follows the render")];
    // rustfmt breaks the call's arguments onto lines of their own.
    let render = render.split_whitespace().collect::<String>();
    assert!(
        render.contains("code_lines_visible(&approval.sql"),
        "{render}"
    );
    assert!(!render.contains("agent.log"));
    assert!(!render.contains("code_lines(&"));
}

#[gpui::test]
fn the_approval_states_its_facts_and_how_many_more_wait(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(pending_calls(&workspace, cx).len(), 2);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(cx.debug_bounds("agent-approval-facts").is_some());
        assert!(
            cx.debug_bounds("agent-approval-waiting").is_some(),
            "two wait and the count says so"
        );

        assert!(workspace.update(cx, |workspace, cx| workspace.deny_front_approval(cx)));
        settle(cx);
        assert_eq!(pending_calls(&workspace, cx).len(), 1);
        assert!(cx.debug_bounds("agent-approval-facts").is_some());
        assert!(
            cx.debug_bounds("agent-approval-waiting").is_none(),
            "nothing waits behind the last"
        );
        quit(workspace, cx);
    });
}

/// A statement whose first frame is past the wire's limit never reaches a
/// dialog: the app's socket server closes the connection, and the shim refuses
/// the call before it opens one. A statement within the limit is shown.
#[cfg(unix)]
#[gpui::test]
fn a_statement_past_the_frame_limit_is_refused_before_any_dialog(cx: &mut TestAppContext) {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let update =
            |filler: usize| format!("UPDATE t SET email = '{}' WHERE id = 1", "x".repeat(filler));
        let nothing_happened = |cx: &mut VisualTestContext| {
            settle(cx);
            assert!(pending_calls(&workspace, cx).is_empty());
            assert_eq!(front_dialog(&workspace, cx), None);
            assert!(workspace.read_with(cx, |workspace, _| workspace.agent.log.is_empty()));
            assert!(audit_lines().is_empty());
            assert_eq!(email_of(1).as_deref(), Some("a@x"));
        };

        // The app's own boundary, which every client meets: 64 KiB is
        // `wire::FRAME_LIMIT`, and this is a first frame past it.
        let sql = update(70_000);
        let long = format!(
            "{}\n",
            frame(&format!(
                r#""kind":"call","tool":"execute_sql","connection":"Orders","sql":"{sql}""#
            ))
        );
        assert!(long.len() > 64 * 1024);
        let mut client = UnixStream::connect(&path).expect("the socket accepts a client");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the timeout is set");
        // The server stops reading at its limit and closes, so the tail of
        // the write may meet a closed socket.
        let _ = client.write_all(long.as_bytes());
        let mut heard = Vec::new();
        // A reset is how some systems report a close with bytes unread.
        let _ = client.read_to_end(&mut heard);
        assert!(heard.is_empty(), "the app answered an oversized frame");
        nothing_happened(cx);

        // The same call through the shim, which refuses it first.
        let shim = crate::agent::shim::testing::call_through_the_shim(
            path.clone(),
            "execute_sql",
            serde_json::json!({ "connection": "Orders", "sql": sql }),
        );
        drive_until(cx, |_| shim.is_finished());
        let refused = shim.join().expect("the shim's thread ends");
        assert_eq!(refused["code"], -32602, "{refused}");
        let message = refused["message"].as_str().unwrap_or_default();
        assert!(message.contains("65536"), "{message}");
        nothing_happened(cx);

        // A statement within the limit is the front approval, whole.
        let within = crate::agent::shim::testing::call_through_the_shim(
            path,
            "execute_sql",
            serde_json::json!({ "connection": "Orders", "sql": update(60_000) }),
        );
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(cx.debug_bounds("agent-approval-size").is_some());
        assert!(workspace.update(cx, |workspace, cx| workspace.deny_front_approval(cx)));
        drive_until(cx, |_| within.is_finished());
        let answered = within.join().expect("the shim's thread ends");
        assert_eq!(answered["isError"], true, "{answered}");
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approval_for_a_profile_hidden_in_another_window_is_not_run(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (_, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        save_visibility_as_another_window("orders", false);
        approve_front(cx);
        assert_eq!(
            answer_of(cx, &answers),
            Reply {
                status: Status::Rejected,
                text: super::agent::NO_LONGER_AVAILABLE.to_string(),
            }
        );
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approved_write_touches_no_tab_grid_buffer_or_history(cx: &mut TestAppContext) {
    use crate::{agent::Status, session::ProfileState};

    store::tests::with_home(|| {
        // `Secret`, the first profile, is connected at launch: the user's own
        // session on the file the agent writes.
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("a connected profile opens on a query tab")
                .editor
                .clone();
            editor.update(cx, |input, cx| input.set_value("SELECT 1", window, cx));
        });
        let record = |cx: &mut VisualTestContext| {
            let shape = workspace.read_with(cx, |workspace, _| {
                let profile = workspace.profile().expect("a profile is in front");
                (
                    profile.id.clone(),
                    profile.session.queries.len(),
                    profile.session.objects.len(),
                    profile.session.active,
                )
            });
            (
                shape,
                buffer(&workspace, cx),
                store::history("secret").len(),
                store::history("orders").len(),
            )
        };
        let before = record(cx);
        assert_eq!(before.0.0, "secret");
        assert_eq!(before.1, "SELECT 1");

        let (_, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);
        settle(cx);

        assert_eq!(record(cx), before);
        workspace.read_with(cx, |workspace, _| {
            let orders = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            assert!(matches!(orders.state, ProfileState::Idle));
        });
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_writes_started_before_the_write_and_the_outcome_after(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        assert!(
            audit_lines().is_empty(),
            "a statement not yet approved is not written"
        );
        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);

        let lines = audit_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let (started, outcome) = (&lines[0], &lines[1]);
        assert_eq!(started["phase"], "started");
        assert_eq!(started["decision"], "approved");
        assert_eq!(started["sql"], UPDATE_ONE);
        assert!(started.get("status").is_none(), "{started}");
        assert_eq!(outcome["phase"], "outcome");
        assert_eq!(outcome["status"], "executed");
        assert_eq!(outcome["sql"], UPDATE_ONE);
        for line in &lines {
            assert_eq!(line["v"], 1);
            assert_eq!(line["id"], id.0);
            assert_eq!(line["session"], started["session"]);
            assert_eq!(line["tool"], "execute_sql");
            assert_eq!(line["profile"], "Orders");
            assert_eq!(line["engine"], Engine::Sqlite.as_str());
            let mut without_sql = line.clone();
            without_sql
                .as_object_mut()
                .expect("a line is an object")
                .remove("sql");
            assert!(
                !without_sql.to_string().contains("z@x"),
                "the statement's text is outside its field: {line}"
            );
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_keeps_an_approved_statement_whole(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let statement = format!("UPDATE t SET email = '{}' WHERE id = 1", "x".repeat(5000));
        assert!(statement.chars().count() > super::agent::AUDIT_SQL_CAP);
        let (_, answers) = inject_approval(&workspace, cx, "Orders", &statement);
        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);

        let lines = audit_lines();
        assert_eq!(lines.len(), 2, "{} lines", lines.len());
        for line in &lines {
            assert_eq!(line["sql"], statement.as_str(), "{}", line["phase"]);
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_failure_runs_nothing(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        // A directory where the file belongs: nothing can be appended to it.
        let log = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join(store::AGENT_AUDIT_FILE);
        std::fs::create_dir_all(&log).expect("the obstacle is creatable");

        let (_, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        approve_front(cx);
        assert_eq!(
            answer_of(cx, &answers),
            Reply {
                status: Status::Failed,
                text: super::agent::NOT_RECORDED.to_string(),
            }
        );
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty(), "a write was started");
            assert!(workspace.agent.lanes.is_empty());
            let entry = workspace.agent.log.front().expect("the call is logged");
            assert_eq!(entry.status, Status::Failed);
        });
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_records_every_call_reads_included(cx: &mut TestAppContext) {
    use crate::agent::{Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let (_, _unknown) = inject_call_with(
            &workspace,
            cx,
            Tool::ExecuteSql,
            Some("Nope"),
            crate::agent::CallArgs {
                sql: Some("SELECT 1".to_string()),
                ..Default::default()
            },
        );
        let (_, listing) = inject_call(&workspace, cx, Tool::ListConnections, None);
        assert_eq!(answer_of(cx, &listing).status, Status::Executed);
        let answers = inject_sql(&workspace, cx, "Orders", "SELECT id FROM t ORDER BY id");
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        let answers = inject_sql(&workspace, cx, "Orders", "DROP TABLE t");
        assert_eq!(answer_of(cx, &answers).status, Status::RefusedByGate);
        let (held, _answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        workspace.update(cx, |workspace, cx| workspace.deny_call(held, cx));

        let lines = audit_lines();
        let shape: Vec<_> = lines
            .iter()
            .map(|line| {
                (
                    line["phase"].as_str().unwrap_or_default().to_string(),
                    line["status"].as_str().map(str::to_string),
                )
            })
            .collect();
        let line =
            |phase: &str, status: Option<&str>| (phase.to_string(), status.map(str::to_string));
        assert_eq!(
            shape,
            [
                line("outcome", Some("refused_by_gate")),
                line("outcome", Some("executed")),
                line("started", None),
                line("outcome", Some("executed")),
                line("outcome", Some("refused_by_gate")),
                line("outcome", Some("rejected")),
            ],
            "{lines:?}"
        );
        assert_eq!(lines[3]["shown"], 3);
        assert_eq!(lines[5]["decision"], "rejected");
        for line in lines.iter().filter(|line| line["phase"] == "started") {
            assert!(line.get("status").is_none(), "{line}");
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_never_records_a_row_value(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let answers = inject_sql(&workspace, cx, "Orders", "SELECT email FROM t WHERE id = 1");
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        let path = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join(store::AGENT_AUDIT_FILE);
        let text = std::fs::read_to_string(path).expect("the read was written");
        assert!(text.contains("SELECT email"), "{text}");
        assert!(
            !text.contains("a@x"),
            "a row value reached the file: {text}"
        );
        quit(workspace, cx);
    });
}

/// A write that opens for `opening` of the test clock and then fails at once,
/// or after `running`, as Redis answers.
fn slow_open_then(
    opening: std::time::Duration,
    running: std::time::Duration,
    executor: &gpui::BackgroundExecutor,
) -> gpui::Task<super::agent_write::Wrote> {
    let executor = executor.clone();
    executor.clone().spawn(async move {
        executor.timer(opening).await;
        super::agent_write::Wrote::Done {
            hidden: Vec::new(),
            ran_for: running,
            result: Err(crate::db::DbError {
                message: "WRONGTYPE Operation against a key holding the wrong kind of value"
                    .to_string(),
                position: None,
            }),
        }
    })
}

#[cfg(unix)]
#[gpui::test]
fn only_a_write_that_ran_its_whole_bound_after_it_was_sent_says_its_outcome_is_not_known(
    cx: &mut TestAppContext,
) {
    use std::time::Duration;

    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("secret", "Secret", ""),
                "[[profiles]]\nid = \"keys\"\nname = \"Keys\"\nhost = \"127.0.0.1\"\n\
                 database = \"0\"\nuser = \"\"\nengine = \"redis\"\nagent_visible = true\n\
                 mode = \"read-write\"\nstatement_timeout = 1\n"
                    .to_string(),
            ]
            .concat(),
        );
        let answer_after = |workspace: &Entity<Workspace>,
                            cx: &mut VisualTestContext,
                            runner: super::agent_write::WriteRunner| {
            workspace.update(cx, |workspace, _| {
                workspace.agent.write_runner = Some(runner)
            });
            let (_, answers) = inject_call_with(
                workspace,
                cx,
                crate::agent::tools::Tool::ExecuteSql,
                Some("Keys"),
                crate::agent::CallArgs {
                    sql: Some("SET k v".to_string()),
                    ..Default::default()
                },
            );
            approve_front(cx);
            cx.executor().advance_clock(Duration::from_secs(5));
            settle(cx);
            let answer = answers.try_recv().expect("the write answered");
            assert_eq!(answer.status, Status::Failed);
            answer.text
        };

        // Most of the bound went on opening; the server then refused it.
        let refused = answer_after(&workspace, cx, |executor, _, _, _, _| {
            slow_open_then(Duration::from_secs(3), Duration::from_millis(5), executor)
        });
        assert!(!refused.contains("is not known"), "{refused}");

        let stopped = answer_after(&workspace, cx, |executor, _, _, _, _| {
            slow_open_then(Duration::ZERO, Duration::from_secs(1), executor)
        });
        assert!(stopped.contains("is not known"), "{stopped}");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_keeps_no_row_value_or_host_from_a_failed_write(cx: &mut TestAppContext) {
    use super::agent_write::Wrote;
    use crate::{agent::Status, db::DbError};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let fail_with = |workspace: &Entity<Workspace>,
                         cx: &mut VisualTestContext,
                         runner: super::agent_write::WriteRunner| {
            workspace.update(cx, |workspace, _| {
                workspace.agent.write_runner = Some(runner)
            });
            let (id, answers) = inject_update(workspace, cx, Some("Orders"));
            approve_front(cx);
            let answer = answer_of(cx, &answers);
            assert_eq!(answer.status, Status::Failed);
            let logged = workspace.read_with(cx, |workspace, _| {
                workspace
                    .agent
                    .log
                    .front()
                    .and_then(|entry| entry.error.clone())
            });
            (id, answer.text, logged)
        };

        let (id, answer, logged) = fail_with(&workspace, cx, |_, _, _, _, _| {
            gpui::Task::ready(Wrote::Done {
                hidden: Vec::new(),
                ran_for: std::time::Duration::ZERO,
                result: Err(DbError {
                    message: "duplicate key value violates unique constraint \"t_email_key\"\n\
                              Key (email)=(a@x) already exists."
                        .to_string(),
                    position: None,
                }),
            })
        });
        assert!(answer.contains("Key (email)=(a@x)"), "{answer}");
        let kept = "duplicate key value violates unique constraint …";
        assert_eq!(logged.as_deref(), Some(kept));
        let outcome = audit_of(id).pop().expect("the outcome was written");
        assert_eq!(outcome["error"], kept);

        let (id, answer, logged) = fail_with(&workspace, cx, |_, _, _, _, _| {
            gpui::Task::ready(Wrote::NotOpened {
                hidden: vec!["db.internal".to_string()],
                error: DbError {
                    message: "could not reach db.internal as 'alice'".to_string(),
                    position: None,
                },
            })
        });
        assert_eq!(answer, super::agent_read::OPEN_FAILED);
        let kept = "could not reach [hidden] as …";
        assert_eq!(logged.as_deref(), Some(kept));
        let outcome = audit_of(id).pop().expect("the outcome was written");
        assert_eq!(outcome["error"], kept);

        let file = serde_json::to_string(&audit_lines()).expect("the lines encode");
        for value in ["a@x", "db.internal", "alice"] {
            assert!(!file.contains(value), "{value} reached the file: {file}");
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_cuts_a_read_and_keeps_an_approval_whole(cx: &mut TestAppContext) {
    use super::agent::{AUDIT_SQL_CAP, SQL_CLIPPED};
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let comment = format!("/* {} */", "x".repeat(5000));
        let read = format!("SELECT id FROM t {comment}");
        let answers = inject_sql(&workspace, cx, "Orders", &read);
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        let cut = format!(
            "{}{SQL_CLIPPED}",
            read.chars().take(AUDIT_SQL_CAP).collect::<String>()
        );
        let lines = audit_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        for line in &lines {
            assert_eq!(line["sql"], cut.as_str(), "{}", line["phase"]);
        }

        let write = format!("{UPDATE_ONE} {comment}");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", &write);
        approve_front(cx);
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        let lines = audit_of(id);
        assert_eq!(lines.len(), 2, "{lines:?}");
        for line in &lines {
            assert_eq!(line["sql"], write.as_str(), "{}", line["phase"]);
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn agent_audit_pairs_lines_by_call_in_the_order_they_happened(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        cx.executor().allow_parking();
        let (a, write) = running_write(&workspace, cx);
        let read = inject_sql(&workspace, cx, "Orders", "SELECT id FROM t");
        assert_eq!(answer_of(cx, &read).status, Status::Executed);
        land_write(cx);
        assert_eq!(answer_of(cx, &write).status, Status::Executed);

        let lines = audit_lines();
        let order: Vec<_> = lines
            .iter()
            .map(|line| {
                (
                    line["id"] == a.0,
                    line["phase"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let step = |is_a: bool, phase: &str| (is_a, phase.to_string());
        assert_eq!(
            order,
            [
                step(true, "started"),
                step(false, "started"),
                step(false, "outcome"),
                step(true, "outcome"),
            ],
            "{lines:?}"
        );
        for line in &lines {
            assert_eq!(line["session"], lines[0]["session"]);
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn enter_never_approves_an_approval(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(ringed_button(&workspace, cx), 0);

        for key in ["right", "left", "tab"] {
            press(cx, key);
            assert_eq!(
                front_dialog(&workspace, cx),
                Some("agent-approval"),
                "{key}"
            );
            assert_eq!(ringed_button(&workspace, cx), 0, "{key}");
            assert_eq!(pending_calls(&workspace, cx), [id], "{key}");
            assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty), "{key}");
        }

        press(cx, "enter");
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(editor_has_keyboard(&workspace, cx));

        let (_, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        press(cx, "escape");
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_closed_approval_hands_the_keyboard_back(cx: &mut TestAppContext) {
    use crate::agent::{EXPIRY, Inbound, Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");

        // Approved.
        let (_, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        approve_front(cx);
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));

        // Denied.
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        workspace.update(cx, |workspace, cx| workspace.deny_call(id, cx));
        settle(cx);
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));

        // Hung up.
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id }, cx)
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(
            answers.try_recv().is_err(),
            "a hung-up call is sent nothing"
        );
        assert!(editor_has_keyboard(&workspace, cx));

        // Expired.
        let (_, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        cx.executor()
            .advance_clock(EXPIRY + std::time::Duration::from_secs(1));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(
            answers.try_recv().ok().map(|reply| reply.status),
            Some(Status::Expired)
        );
        assert!(editor_has_keyboard(&workspace, cx));

        // Cut off. No bridge runs here: the kill switch answers every pending
        // call first, with or without one.
        let (_, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        run_command(&workspace, cx, Command::DisconnectAgents);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(
            answers.try_recv().ok(),
            Some(Reply {
                status: Status::Rejected,
                text: "Agent access was turned off in DBDelve.".to_string(),
            })
        );
        assert!(editor_has_keyboard(&workspace, cx));

        // Hidden from agents: the profile is the second, after `Secret`.
        let (_, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        run_command(&workspace, cx, Command::HideFromAgents(1));
        settle(cx);
        assert_eq!(answers.try_recv().ok(), Some(swept()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        assert_eq!(email_of(1).as_deref(), Some("z@x"), "only the approved ran");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn approvals_are_shown_oldest_first(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let (first, first_answers) = inject_update(&workspace, cx, Some("Orders"));
        let (second, second_answers) = inject_update(&workspace, cx, Some("Orders"));
        let (third, third_answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(front_approval_id(&workspace, cx), Some(first));

        workspace.update(cx, |workspace, cx| workspace.deny_call(first, cx));
        settle(cx);
        assert_eq!(first_answers.try_recv().ok(), Some(denied()));
        assert_eq!(pending_calls(&workspace, cx), [second, third]);
        assert_eq!(front_approval_id(&workspace, cx), Some(second));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(
            second_answers.try_recv().err(),
            Some(TryRecvError::Empty),
            "the second approval was answered by the first's denial"
        );

        workspace.update(cx, |workspace, cx| workspace.deny_call(second, cx));
        settle(cx);
        assert_eq!(second_answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_approval_id(&workspace, cx), Some(third));
        assert_eq!(third_answers.try_recv().err(), Some(TryRecvError::Empty));
        quit(workspace, cx);
    });
}

fn swept() -> crate::agent::Reply {
    crate::agent::Reply {
        status: crate::agent::Status::Rejected,
        text: "The connection is no longer available to agents.".to_string(),
    }
}

#[gpui::test]
fn hiding_a_profile_rejects_its_pending_calls_and_no_others(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let visible = VISIBLE_RW;
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("a", "Shop", visible),
                sqlite_profile("b", "Shop", ""),
                sqlite_profile("o", "Orders", visible),
                sqlite_profile("r", "Reports", visible),
            ]
            .concat(),
        );
        let (first_shop, first_shop_answers) = inject_update(&workspace, cx, Some("Shop"));
        let (orders, orders_answers) = inject_update(&workspace, cx, Some("Orders"));
        let (second_shop, second_shop_answers) = inject_update(&workspace, cx, Some("Shop"));
        let (reports, reports_answers) = inject_update(&workspace, cx, Some("Reports"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(front_approval_id(&workspace, cx), Some(first_shop));

        // Hiding the profile that is already hidden answers nothing.
        workspace.update(cx, |workspace, cx| {
            workspace.set_agent_visible("b", false, cx)
        });
        assert_eq!(
            pending_calls(&workspace, cx),
            [first_shop, orders, second_shop, reports]
        );

        run_command(&workspace, cx, Command::HideFromAgents(0));
        assert_eq!(first_shop_answers.try_recv().ok(), Some(swept()));
        assert_eq!(second_shop_answers.try_recv().ok(), Some(swept()));
        assert!(first_shop_answers.try_recv().is_err(), "answered twice");
        assert!(second_shop_answers.try_recv().is_err(), "answered twice");
        assert_eq!(pending_calls(&workspace, cx), [orders, reports]);
        assert_eq!(orders_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(reports_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(front_approval_id(&workspace, cx), Some(orders));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));

        // Nothing waits on Reports once its approval is denied, so hiding it sends
        // nothing and says nothing.
        workspace.update(cx, |workspace, cx| workspace.deny_call(orders, cx));
        workspace.update(cx, |workspace, cx| workspace.deny_call(reports, cx));
        assert_eq!(orders_answers.try_recv().ok(), Some(denied()));
        assert_eq!(reports_answers.try_recv().ok(), Some(denied()));
        workspace.update(cx, |workspace, _| workspace.clear_notice());
        run_command(&workspace, cx, Command::HideFromAgents(3));
        assert!(!agent_visible(&workspace, cx, "r"));
        assert!(reports_answers.try_recv().is_err());
        assert_eq!(shown_notice(&workspace, cx), None);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn removing_a_profile_rejects_its_pending_calls(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let visible = VISIBLE_RW;
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("o", "Orders", visible),
                sqlite_profile("r", "Reports", visible),
            ]
            .concat(),
        );
        let (orders, orders_answers) = inject_update(&workspace, cx, Some("Orders"));
        let (reports, reports_answers) = inject_update(&workspace, cx, Some("Reports"));
        settle(cx);
        assert_eq!(front_approval_id(&workspace, cx), Some(orders));

        workspace.update(cx, |workspace, cx| workspace.remove_profile(0, cx));
        settle(cx);
        assert_eq!(orders_answers.try_recv().ok(), Some(swept()));
        assert_eq!(pending_calls(&workspace, cx), [reports]);
        assert_eq!(reports_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(front_approval_id(&workspace, cx), Some(reports));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
fn agent_mark_state(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::ui::AgentMark> {
    workspace.read_with(cx, |workspace, _| workspace.agent_mark_state())
}

/// Whether the mark drew itself in the last frame, in either state.
#[cfg(unix)]
fn agent_mark_drawn(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("agent-mark-listening").is_some()
        || cx.debug_bounds("agent-mark-connected").is_some()
}

#[cfg(unix)]
#[gpui::test]
fn the_agent_mark_is_absent_off_dim_listening_and_lit_when_connected(cx: &mut TestAppContext) {
    use crate::{
        agent::{ConnId, Inbound},
        ui::AgentMark,
    };

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.profile().is_some()));

        assert!(!agent_mark_drawn(cx), "access is off");
        assert_eq!(agent_mark_state(&workspace, cx), None);

        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));
        assert!(cx.debug_bounds("agent-mark-listening").is_some());
        assert!(cx.debug_bounds("agent-mark-connected").is_none());

        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::PresenceOpened { conn: ConnId(1) }, cx)
        });
        settle(cx);
        assert_eq!(
            agent_mark_state(&workspace, cx),
            Some(AgentMark::Connected { waiting: 0 })
        );
        assert!(cx.debug_bounds("agent-mark-connected").is_some());
        assert!(cx.debug_bounds("agent-mark-listening").is_none());

        let (id, _answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(
            agent_mark_state(&workspace, cx),
            Some(AgentMark::Connected { waiting: 1 })
        );

        workspace.update(cx, |workspace, cx| workspace.deny_call(id, cx));
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::PresenceClosed { conn: ConnId(1) }, cx)
        });
        settle(cx);
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));
        assert!(cx.debug_bounds("agent-mark-listening").is_some());

        run_command(&workspace, cx, Command::DisconnectAgents);
        assert_eq!(agent_mark_state(&workspace, cx), None);
        assert!(!agent_mark_drawn(cx), "the kill switch takes the mark away");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_agent_mark_is_in_every_titlebar(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // No saved profile: the welcome surface, unless the runner's `PG*`
        // variables put a profile in front, which has the main titlebar.
        let (workspace, cx) = launch_with_settings(cx, "agent_access = true", "");
        cx.executor().allow_parking();
        assert!(cx.debug_bounds("agent-mark-listening").is_some());
        quit(workspace, cx);

        // A profile in front, then the connection form over it.
        let (workspace, cx) = launch_with_settings(
            cx,
            "agent_access = true",
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.profile().is_some()));
        assert!(cx.debug_bounds("agent-mark-listening").is_some());

        run_command(&workspace, cx, Command::NewConnection);
        assert!(workspace.read_with(cx, |workspace, _| workspace.form.is_some()));
        assert!(cx.debug_bounds("agent-mark-listening").is_some());
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_agent_mark_lights_when_a_running_shim_finds_access_turned_on(cx: &mut TestAppContext) {
    use std::time::Duration;

    use crate::ui::AgentMark;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let presence = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.agent.presence.len())
        };

        // Claude Code already has DBDelve loaded: the shim runs and retries
        // against a socket nothing listens at.
        let shim = crate::agent::shim::testing::idle_shim(path, Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(200));
        settle(cx);
        assert!(!agent_mark_drawn(cx));
        assert_eq!(presence(cx), 0);

        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        drive_until(cx, |cx| {
            agent_mark_state(&workspace, cx) == Some(AgentMark::Connected { waiting: 0 })
        });
        // No call was made: the agent is there because its shim is.
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(shim.lines_after_handshake().is_empty());
        settle(cx);
        assert!(cx.debug_bounds("agent-mark-connected").is_some());

        run_command(&workspace, cx, Command::DisconnectAgents);
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        assert!(!agent_mark_drawn(cx));
        assert_eq!(presence(cx), 0);
        assert!(
            !listening(&workspace, cx),
            "the shim's retries bound nothing"
        );

        drop(shim);
        quit(workspace, cx);
    });
}

fn agent_log(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<super::agent::LogEntry> {
    workspace.read_with(cx, |workspace, _| {
        workspace.agent.log.iter().cloned().collect()
    })
}

#[gpui::test]
fn every_finished_call_is_logged_once_and_the_log_is_capped(cx: &mut TestAppContext) {
    use std::time::Duration;

    use crate::agent::{EXPIRY, Inbound, Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let (listed, _listed_answers) = inject_call(&workspace, cx, Tool::ListConnections, None);
        // A name the agent made up, with a bell in it and far too long.
        let made_up = format!("Nope\u{7}{}", "x".repeat(80));
        let (refused, _refused_answers) = inject_update(&workspace, cx, Some(&made_up));
        let (expired, _expired_answers) = inject_update(&workspace, cx, Some("Orders"));
        cx.executor().advance_clock(EXPIRY + Duration::from_secs(1));
        cx.run_until_parked();
        let (hung_up, _hung_up_answers) = inject_update(&workspace, cx, Some("Orders"));
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id: hung_up }, cx)
        });
        let (denied, _denied_answers) = inject_update(&workspace, cx, Some("Orders"));
        workspace.update(cx, |workspace, cx| workspace.deny_call(denied, cx));

        let log = agent_log(&workspace, cx);
        assert_eq!(
            log.iter().map(|entry| entry.status).collect::<Vec<_>>(),
            [
                Status::Rejected,
                Status::Cancelled,
                Status::Expired,
                Status::RefusedByGate,
                Status::Executed
            ]
        );
        assert_eq!(
            log.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [denied, hung_up, expired, refused, listed]
        );
        let asked = log[3]
            .asked
            .as_deref()
            .expect("the refusal keeps what was asked");
        assert_eq!(asked.chars().count(), 64);
        assert!(asked.starts_with("Nope") && !asked.chars().any(char::is_control));
        assert_eq!(log[3].resolved, None);
        for entry in &log[..3] {
            assert_eq!(entry.resolved.as_deref(), Some("Orders"));
            assert_eq!(entry.asked, None);
        }
        // What `list_connections` answered is not kept: not the text, nor a
        // name it listed.
        assert_eq!((&log[4].resolved, &log[4].asked), (&None, &None));
        assert!(!format!("{:?}", log[4]).contains("engine"));

        for _ in 0..25 {
            inject_call(&workspace, cx, Tool::ListConnections, None);
        }
        // Thirty entries are held; one fewer than the cap evicts nothing, and
        // the oldest, `listed`, is still the last of them.
        for _ in 0..(super::agent::AUDIT_CAP - 1 - 5 - 25) {
            inject_call(&workspace, cx, Tool::ListConnections, None);
        }
        let log = agent_log(&workspace, cx);
        assert_eq!(log.len(), super::agent::AUDIT_CAP - 1);
        assert_eq!(log.last().map(|entry| entry.id), Some(listed));
        inject_call(&workspace, cx, Tool::ListConnections, None);
        let log = agent_log(&workspace, cx);
        assert_eq!(log.len(), super::agent::AUDIT_CAP);
        assert_eq!(log.last().map(|entry| entry.id), Some(listed));
        // The next call is the one that evicts, and it evicts the oldest.
        inject_call(&workspace, cx, Tool::ListConnections, None);
        let log = agent_log(&workspace, cx);
        assert_eq!(log.len(), super::agent::AUDIT_CAP);
        assert!(log.iter().all(|entry| entry.id != listed));
        let mut ids: Vec<_> = log.iter().map(|entry| entry.id.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            super::agent::AUDIT_CAP,
            "a call was logged twice"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_answer_nobody_was_waiting_for_is_logged_as_having_no_requester(cx: &mut TestAppContext) {
    use crate::agent::{CallId, Inbound, Status};

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let (id, _answers) = inject_update(&workspace, cx, Some("Orders"));
        workspace.update(cx, |workspace, cx| workspace.deny_call(id, cx));
        let log = agent_log(&workspace, cx);
        assert_eq!(
            (log.len(), log[0].status, log[0].requester_gone),
            (1, Status::Rejected, false)
        );

        // An id nothing logged is not a call this window answered.
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(
                Inbound::Undelivered {
                    id: CallId(u64::MAX),
                },
                cx,
            )
        });
        let log = agent_log(&workspace, cx);
        assert_eq!((log.len(), log[0].requester_gone), (1, false));

        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Undelivered { id }, cx)
        });
        let log = agent_log(&workspace, cx);
        assert_eq!(
            (log.len(), log[0].status, log[0].requester_gone),
            (1, Status::Rejected, true)
        );
        quit(workspace, cx);
    });
}

#[test]
fn a_log_line_reads_age_tool_name_code_and_whether_anyone_was_waiting() {
    use super::{agent::LogEntry, agent_log_label};
    use crate::agent::{CallId, Status, tools::Tool};

    let entry = LogEntry {
        at: 1_000_000,
        id: CallId(1),
        tool: Tool::ExecuteSql,
        resolved: None,
        asked: Some("Nope".to_string()),
        status: Status::RefusedByGate,
        requester_gone: false,
        verdict: None,
        sql: None,
        shown: None,
        total: None,
        elapsed_ms: 0,
        error: None,
        decision: None,
    };
    assert_eq!(
        agent_log_label(&entry, 1_000_000 + 125_000),
        "2m ago \u{b7} execute_sql \u{b7} asked for \"Nope\" \u{b7} refused_by_gate"
    );
    let entry = LogEntry {
        resolved: Some("Orders".to_string()),
        asked: None,
        status: Status::Executed,
        requester_gone: true,
        ..entry
    };
    assert_eq!(
        agent_log_label(&entry, 1_000_000),
        "moments ago \u{b7} execute_sql \u{b7} Orders \u{b7} executed \u{b7} no requester"
    );
    let entry = LogEntry {
        resolved: None,
        requester_gone: false,
        tool: Tool::ListConnections,
        ..entry
    };
    assert_eq!(
        agent_log_label(&entry, 1_000_000),
        "moments ago \u{b7} list_connections \u{b7} executed"
    );
    let entry = LogEntry {
        resolved: Some("Orders".to_string()),
        tool: Tool::ExecuteSql,
        decision: Some(super::agent::Decision::Approved),
        ..entry
    };
    assert_eq!(
        agent_log_label(&entry, 1_000_000),
        "moments ago \u{b7} execute_sql \u{b7} Orders \u{b7} executed \u{b7} approved"
    );
}

#[gpui::test]
fn every_agent_call_is_audited_with_its_verdict_and_never_its_rows(cx: &mut TestAppContext) {
    use crate::agent::{Status, result::Total, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('secret-cell-value');",
            "",
        );
        cx.executor().allow_parking();
        let sql = "SELECT v FROM t";
        let (id, answers) = inject_call_with(
            &workspace,
            cx,
            Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some(sql.to_string()),
                ..Default::default()
            },
        );
        let reply = answer_of(cx, &answers);
        assert_eq!(reply.status, Status::Executed, "{}", reply.text);
        assert!(reply.text.contains("secret-cell-value"), "{}", reply.text);
        inject_call(&workspace, cx, Tool::ListConnections, None);

        let log = agent_log(&workspace, cx);
        let read = log
            .iter()
            .find(|entry| entry.id == id)
            .expect("the read is audited");
        assert_eq!(read.tool, Tool::ExecuteSql);
        assert_eq!(read.resolved.as_deref(), Some("Orders"));
        assert_eq!(read.status, Status::Executed);
        assert_eq!(read.verdict, Some(super::agent::AuditVerdict::Run));
        assert_eq!(read.sql.as_deref(), Some(sql));
        assert_eq!((read.shown, read.total), (Some(1), Some(Total::Exact(1))));
        assert!(!read.capped());
        assert_eq!(read.error, None);
        for entry in &log {
            assert!(
                !format!("{entry:?}").contains("secret-cell-value"),
                "a row value reached the audit: {entry:?}"
            );
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refused_or_approval_needing_call_is_audited_with_its_reason(cx: &mut TestAppContext) {
    use super::agent::AuditVerdict;
    use crate::{actions::ShowAgentActivity, agent::Status, sql::Mode};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let insert = "INSERT INTO t VALUES (9, 'z')";
        let drop = "DROP TABLE t";
        let (held, _held_answers) = inject_approval(&workspace, cx, "Orders", insert);
        inject_sql(&workspace, cx, "Orders", drop);
        cx.run_until_parked();

        // The refusal is logged at once; the held statement waits for a person.
        let log = agent_log(&workspace, cx);
        let refusal = "DROP is refused for agent statements";
        assert_eq!(log.len(), 1);
        assert_eq!(
            (log[0].status, log[0].verdict.clone(), log[0].sql.as_deref()),
            (
                Status::RefusedByGate,
                Some(AuditVerdict::Refuse(refusal.to_string())),
                Some(drop)
            )
        );
        workspace.update(cx, |workspace, cx| workspace.deny_call(held, cx));

        let log = agent_log(&workspace, cx);
        // Newest first: the denied INSERT, then the DROP.
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[0].status, log[0].verdict.clone(), log[0].sql.as_deref()),
            (
                Status::Rejected,
                Some(AuditVerdict::Approve {
                    mode: Mode::ReadWrite,
                    expiry: None
                }),
                Some(insert)
            )
        );
        for entry in &log {
            assert_eq!((entry.shown, entry.total), (None, None), "{entry:?}");
        }

        assert_eq!(
            log[1].verdict.as_ref().map(|v| v.line()),
            Some(format!("Refused: {refusal}"))
        );
        assert_eq!(
            log[0].verdict.as_ref().map(|v| v.line()).as_deref(),
            Some("Needs approval at Read-write")
        );
        // The dialog draws both entries with their statements.
        cx.update(|window, cx| window.dispatch_action(Box::new(ShowAgentActivity), cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-audit"));
        assert!(cx.debug_bounds("agent-audit-sql").is_some());
        quit(workspace, cx);
    });
}

#[test]
fn an_entry_is_capped_when_the_agent_was_given_fewer_rows_than_there_were() {
    use super::agent::LogEntry;
    use crate::agent::{CallId, Status, result::Total, tools::Tool};

    let entry = LogEntry {
        at: 0,
        id: CallId(1),
        tool: Tool::ExecuteSql,
        resolved: None,
        asked: None,
        status: Status::Executed,
        requester_gone: false,
        verdict: None,
        sql: None,
        shown: Some(100),
        total: Some(Total::Exact(100)),
        elapsed_ms: 0,
        error: None,
        decision: None,
    };
    assert!(!entry.capped());
    let more = LogEntry {
        total: Some(Total::Exact(101)),
        ..entry.clone()
    };
    assert!(more.capped());
    let stopped = LogEntry {
        total: Some(Total::AtLeast(100)),
        ..entry.clone()
    };
    assert!(stopped.capped());
    let none = LogEntry {
        shown: None,
        total: None,
        ..entry
    };
    assert!(!none.capped());
}

#[cfg(unix)]
#[gpui::test]
fn the_agent_audit_opens_from_the_popover_and_hands_the_keyboard_back(cx: &mut TestAppContext) {
    use crate::{actions::ShowAgentActivity, agent::Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));

        // What the popover's item dispatches.
        let open = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.dispatch_action(Box::new(ShowAgentActivity), cx));
            settle(cx);
        };
        open(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-audit"));
        assert!(dialog_has_keyboard(&workspace, cx));
        assert!(
            cx.debug_bounds("agent-audit-empty").is_some(),
            "an empty audit says so"
        );
        press(cx, "escape");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));

        let sql = "SELECT id FROM t";
        let answers = inject_sql(&workspace, cx, "Orders", sql);
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        settle(cx);
        open(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-audit"));
        assert!(cx.debug_bounds("agent-audit-empty").is_none());
        assert!(
            cx.debug_bounds("agent-audit-sql").is_some(),
            "the statement is drawn"
        );
        assert_eq!(
            agent_log(&workspace, cx)[0].sql.as_deref(),
            Some(sql),
            "what is drawn is what the agent sent"
        );
        // Its one button closes it too.
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_audit_lists_calls_newest_first_by_when_they_ended(cx: &mut TestAppContext) {
    use std::time::Instant;

    use crate::agent::{Inbound, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let began = Instant::now();
        let (first, _first_answers) = inject_call(&workspace, cx, Tool::ListConnections, None);
        let (second, _second_answers) = inject_call(&workspace, cx, Tool::ListConnections, None);
        let (third, _third_answers) = inject_update(&workspace, cx, Some("Orders"));
        workspace.update(cx, |workspace, cx| workspace.deny_call(third, cx));
        let ended: Vec<_> = agent_log(&workspace, cx)
            .iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(ended, [third, second, first]);

        // A hangup for a call that is over adds nothing and moves nothing.
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id: first }, cx)
        });
        let after: Vec<_> = agent_log(&workspace, cx)
            .iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(after, ended);

        let waited = began.elapsed().as_millis() as u64;
        for entry in agent_log(&workspace, cx) {
            assert!(entry.elapsed_ms <= waited, "{entry:?} after {waited} ms");
        }
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_agent_popover_turns_access_off(cx: &mut TestAppContext) {
    use crate::agent::{ConnId, Inbound};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::PresenceOpened { conn: ConnId(1) }, cx)
        });
        settle(cx);
        let in_menu = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("PopupMenu"))
            })
        };
        assert!(!in_menu(cx));

        let mark = cx
            .debug_bounds("agent-mark-connected")
            .expect("an attached agent lights the mark");
        cx.simulate_click(mark.center(), gpui::Modifiers::default());
        settle(cx);
        assert!(in_menu(cx), "clicking the mark opened no menu");

        // The kill switch is the only item that can be chosen. The menu's
        // first step down always lands on its first row, a label, so the
        // second is the one that reaches the item.
        cx.simulate_keystrokes("down");
        settle(cx);
        assert!(listening(&workspace, cx), "a label was chosen");
        cx.simulate_keystrokes("down enter");
        settle(cx);
        assert!(!in_menu(cx));
        assert!(!listening(&workspace, cx));
        assert!(workspace.read_with(cx, |workspace, _| workspace.agent.presence.is_empty()));
        assert!(!agent_mark_drawn(cx));
        assert_eq!(saved_agent_access(), Some(false));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn turning_access_off_from_the_mark_while_an_approval_waits_rejects_it_and_closes_the_dialog(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let (_, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));

        // The dialog stops under the titlebar, so the click reaches the mark.
        let mark = cx
            .debug_bounds("agent-mark-connected")
            .expect("a waiting call lights the mark");
        cx.simulate_click(mark.center(), gpui::Modifiers::default());
        settle(cx);
        let menu_open = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("PopupMenu"))
            })
        };
        assert!(
            menu_open(cx),
            "the dialog took the click meant for the mark"
        );
        assert_eq!(
            answers.try_recv().err(),
            Some(std::sync::mpsc::TryRecvError::Empty),
            "opening the menu answered the approval"
        );

        // By the pointer: the dialog holds the keyboard while it is up, so a
        // key meant for the menu is the dialog's, and `enter` there denies.
        // The rows have no selector, so each try reopens the menu and clicks
        // one row lower until the one item that acts is under the pointer; a
        // click on a label or outside the menu only closes it.
        for step in 0..12 {
            if pending_calls(&workspace, cx).is_empty() {
                break;
            }
            if !menu_open(cx) {
                cx.simulate_click(mark.center(), gpui::Modifiers::default());
                settle(cx);
            }
            let at = gpui::point(
                mark.center().x,
                mark.bottom() + gpui::px(14. + 8. * step as f32),
            );
            cx.simulate_click(at, gpui::Modifiers::default());
            settle(cx);
        }
        assert_eq!(
            answers.try_recv().ok(),
            Some(crate::agent::Reply {
                status: crate::agent::Status::Rejected,
                text: "Agent access was turned off in DBDelve.".to_string(),
            })
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(!listening(&workspace, cx));
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_click_on_the_approval_backdrop_below_the_titlebar_does_not_approve(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_with_profiles(cx, &sqlite_profile("orders", "Orders", VISIBLE_RW));
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));

        let just_below_the_titlebar = gpui::point(
            gpui::px(12.),
            gpui::px(crate::theme::layout::TITLEBAR_HEIGHT + 12.),
        );
        cx.simulate_click(just_below_the_titlebar, gpui::Modifiers::default());
        settle(cx);

        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn another_windows_visibility_is_answered_and_kept_and_only_this_windows_own_choice_overrides_it(
    cx: &mut TestAppContext,
) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("orders", "Orders", "agent_visible = true"),
                sqlite_profile("billing", "Billing", ""),
            ]
            .concat(),
        );
        cx.executor().allow_parking();

        // Another window hides Orders and, through its own dialog, shows
        // Billing. Only the file says so.
        let file = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("profiles.toml");
        let text = std::fs::read_to_string(&file).expect("profiles.toml reads");
        let text = text.replace("agent_visible = true\n", "").replace(
            "name = \"Billing\"\n",
            "name = \"Billing\"\nagent_visible = true\n",
        );
        std::fs::write(&file, text).expect("profiles.toml is written");
        assert!(saved_visible("billing") && !saved_visible("orders"));

        // The window serving agents answers with the file's flags.
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        let client = raw_call(&path, &frame(r#""kind":"call","tool":"list_connections""#));
        drive_until(cx, |_| client.is_finished());
        let answer = client.join().expect("the client thread finished");
        let reply: Reply = serde_json::from_str(answer.trim_end()).expect("one answer frame");
        assert_eq!(reply.status, Status::Executed);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply.text).expect("a JSON array"),
            serde_json::json!([{"name": "Billing", "engine": "sqlite"}])
        );

        // A save for something else entirely writes the other window's flags
        // back as they are.
        workspace.update(cx, |workspace, cx| workspace.remember_profiles(cx));
        assert!(!saved_visible("orders") && saved_visible("billing"));
        assert!(!agent_visible(&workspace, cx, "orders"));
        assert!(agent_visible(&workspace, cx, "billing"));

        // This window's own hide is the one that is written.
        workspace.update(cx, |workspace, cx| {
            workspace.set_agent_visible("billing", false, cx)
        });
        assert!(!saved_visible("billing"));

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_agent_reads_rows_through_the_shim_on_a_connection_of_its_own(cx: &mut TestAppContext) {
    use crate::session::ProfileState;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let untouched = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let orders = workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == "orders")
                    .expect("the profile was restored");
                (
                    matches!(orders.state, ProfileState::Idle),
                    orders.session.queries.len(),
                    orders.session.objects.len(),
                )
            })
        };
        let before = untouched(cx);
        assert!(before.0, "the user never connected Orders");

        let shim = crate::agent::shim::testing::call_through_the_shim(
            path,
            "execute_sql",
            serde_json::json!({
                "connection": "Orders",
                "sql": "SELECT id, email FROM t ORDER BY id",
            }),
        );
        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a result carries text");
        assert!(
            text.starts_with("status: executed\ntruncated: none (3 rows)\ncolumns: "),
            "{text}"
        );
        assert!(text.contains("[\"1\","), "{text}");
        assert!(text.contains("null]"), "{text}");

        assert_eq!(
            untouched(cx),
            before,
            "the agent's read moved the user's profile"
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.calls.is_empty(), "a read asks nobody");
            assert_eq!(workspace.agent.lanes.len(), 1);
            let orders = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            let lane = workspace
                .agent
                .lanes
                .get("orders")
                .expect("the read opened a lane for Orders");
            assert!(lane.opened_from == orders.config);
        });

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_agent_walks_a_catalog_through_the_shim(cx: &mut TestAppContext) {
    use crate::{
        agent::{Status, tools::Tool},
        session::ProfileState,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "CREATE TABLE a (id INTEGER PRIMARY KEY);\
             CREATE TABLE b (id INTEGER PRIMARY KEY);\
             CREATE VIEW v AS SELECT id FROM a;",
            "",
        );
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let untouched = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let orders = workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == "orders")
                    .expect("the profile was restored");
                (
                    matches!(orders.state, ProfileState::Idle),
                    orders.session.queries.len(),
                    orders.session.objects.len(),
                )
            })
        };
        let before = untouched(cx);
        assert!(before.0, "the user never connected Orders");
        let mut call = |tool: &str, arguments: serde_json::Value| {
            let shim =
                crate::agent::shim::testing::call_through_the_shim(path.clone(), tool, arguments);
            drive_until(cx, |_| shim.is_finished());
            let result = shim.join().expect("the shim's thread ends");
            (
                result["isError"] == true,
                result["content"][0]["text"]
                    .as_str()
                    .expect("a result carries text")
                    .to_string(),
            )
        };

        let (failed, schemas) = call("list_schemas", serde_json::json!({"connection": "Orders"}));
        assert!(!failed, "{schemas}");
        assert!(
            schemas.starts_with("status: executed\ntruncated: none (1 row)\ncolumns: "),
            "{schemas}"
        );
        assert!(schemas.ends_with("\n[\"main\"]\n"), "{schemas}");

        let (failed, tables) = call(
            "list_tables",
            serde_json::json!({"connection": "Orders", "schema": "main"}),
        );
        assert!(!failed, "{tables}");
        assert!(
            tables.starts_with("status: executed\ntruncated: none (3 rows)\ncolumns: "),
            "{tables}"
        );
        let rows: Vec<&str> = tables
            .lines()
            .filter(|line| line.starts_with("[\""))
            .collect();
        assert_eq!(rows.len(), 3, "{tables}");
        assert!(rows[0].starts_with("[\"a\",\"table\",null,"), "{tables}");
        assert!(rows[1].starts_with("[\"b\",\"table\",null,"), "{tables}");
        assert!(rows[2].starts_with("[\"v\",\"view\",null,"), "{tables}");

        let (failed, missing) = call(
            "list_tables",
            serde_json::json!({"connection": "Orders", "schema": "nope"}),
        );
        assert!(failed, "{missing}");
        assert_eq!(
            missing,
            format!("status: failed\n{}", super::agent_read::NO_SUCH_SCHEMA)
        );

        let log = agent_log(&workspace, cx);
        assert_eq!(
            log.iter()
                .map(|entry| (entry.tool, entry.status))
                .collect::<Vec<_>>(),
            [
                (Tool::ListTables, Status::Failed),
                (Tool::ListTables, Status::Executed),
                (Tool::ListSchemas, Status::Executed),
            ]
        );
        for entry in &log {
            assert!(entry.verdict.is_none() && entry.sql.is_none(), "{entry:?}");
        }
        assert_eq!(log[1].shown, Some(3));
        assert_eq!(log[2].shown, Some(1));

        assert_eq!(
            untouched(cx),
            before,
            "the agent's catalog read moved the user's profile"
        );

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

const DESCRIBED_SCHEMA: &str = "CREATE TABLE teams (id INTEGER PRIMARY KEY);\
     CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT NOT NULL DEFAULT 'x', \
     team INTEGER REFERENCES teams(id));\
     CREATE UNIQUE INDEX t_email ON t (email);";

#[cfg(unix)]
#[gpui::test]
fn an_agent_describes_a_table_through_the_shim(cx: &mut TestAppContext) {
    use crate::{
        agent::{Status, tools::Tool},
        session::ProfileState,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, DESCRIBED_SCHEMA, "");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let shim = crate::agent::shim::testing::call_through_the_shim(
            path,
            "describe_table",
            serde_json::json!({"connection": "Orders", "schema": "main", "table": "t"}),
        );
        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a result carries text");
        assert!(text.starts_with("status: executed\ntruncated: "), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let after = |label: &str| {
            let at = lines
                .iter()
                .position(|line| line.starts_with(&format!("{label}: ")))
                .unwrap_or_else(|| panic!("no `{label}` table in {text}"));
            &lines[at + 1..]
        };
        let columns = after("columns");
        assert_eq!(columns[0], "[\"id\",\"INTEGER\",\"false\",null]", "{text}");
        assert_eq!(
            columns[1], "[\"email\",\"TEXT\",\"false\",\"'x'\"]",
            "{text}"
        );
        assert_eq!(columns[2], "[\"team\",\"INTEGER\",\"true\",null]", "{text}");
        assert_eq!(after("primary_key")[0], "[\"id\"]", "{text}");
        assert_eq!(
            after("foreign_keys")[0],
            "[\"team\",\"main\",\"teams\",\"id\"]",
            "{text}"
        );
        assert!(
            after("indexes")
                .iter()
                .any(|line| line.starts_with("[\"t_email\",\"unique\",")),
            "{text}"
        );

        let log = agent_log(&workspace, cx);
        assert_eq!(
            log.iter()
                .map(|entry| (entry.tool, entry.status))
                .collect::<Vec<_>>(),
            [(Tool::DescribeTable, Status::Executed)]
        );
        assert!(
            log[0].verdict.is_none() && log[0].sql.is_none(),
            "{:?}",
            log[0]
        );
        workspace.read_with(cx, |workspace, _| {
            let orders = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            assert!(
                matches!(orders.state, ProfileState::Idle),
                "the agent's describe moved the user's profile"
            );
        });
        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_agent_describes_a_table_and_an_unknown_name_reads_as_missing(cx: &mut TestAppContext) {
    use crate::agent::{CallArgs, Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, DESCRIBED_SCHEMA, "");
        cx.executor().allow_parking();
        let mut describe = |schema: &str, table: &str| {
            let (_, answers) = inject_call_with(
                &workspace,
                cx,
                Tool::DescribeTable,
                Some("Orders"),
                CallArgs {
                    schema: Some(schema.to_string()),
                    table: Some(table.to_string()),
                    ..CallArgs::default()
                },
            );
            let mut reply = None;
            drive_until(cx, |_| {
                reply = reply.take().or_else(|| answers.try_recv().ok());
                reply.is_some()
            });
            reply.expect("the call answered")
        };

        let held = describe("main", "t");
        assert_eq!(held.status, Status::Executed, "{}", held.text);
        for (schema, table) in [
            ("main", "nope"),
            ("other", "t"),
            ("MAIN", "t"),
            ("main", "T"),
        ] {
            let missing = describe(schema, table);
            assert_eq!(missing.status, Status::Failed, "{schema}.{table}");
            assert_eq!(
                missing.text,
                super::agent_read::NOT_IN_CATALOG,
                "{schema}.{table}"
            );
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_catalog_call_for_a_hidden_or_unknown_connection_is_refused_and_opens_nothing(
    cx: &mut TestAppContext,
) {
    use crate::agent::{Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        for (tool, name) in [
            (Tool::ListSchemas, "Secret"),
            (Tool::ListTables, "Secret"),
            (Tool::ListSchemas, "Nope"),
        ] {
            let (_, answers) = inject_call_with(
                &workspace,
                cx,
                tool,
                Some(name),
                crate::agent::CallArgs {
                    schema: Some("main".to_string()),
                    ..Default::default()
                },
            );
            let reply = answers.try_recv().expect("a refusal is answered at once");
            assert_eq!(
                reply.status,
                Status::RefusedByGate,
                "{name}: {}",
                reply.text
            );
        }
        workspace.read_with(cx, |workspace, _| assert!(workspace.agent.lanes.is_empty()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_read_keeps_the_first_rows_and_runs_as_written(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT);\
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 150) \
             INSERT INTO t (id) SELECT i FROM n;",
            "",
        );
        cx.executor().allow_parking();
        let mut read = |sql: &str| {
            let answers = inject_sql(&workspace, cx, "Orders", sql);
            let mut reply = None;
            drive_until(cx, |_| {
                reply = reply.take().or_else(|| answers.try_recv().ok());
                reply.is_some()
            });
            reply.expect("the read answered")
        };

        let long = read("SELECT id FROM t ORDER BY id");
        assert_eq!(long.status, Status::Executed, "{}", long.text);
        assert!(
            long.text
                .starts_with("truncated: 100 of at least 100 rows shown (row cap)\ncolumns: "),
            "{}",
            long.text
        );
        let row_lines = |text: &str| text.lines().filter(|line| line.starts_with("[\"")).count();
        assert_eq!(row_lines(&long.text), 100, "{}", long.text);

        let limited = read("SELECT id FROM t ORDER BY id LIMIT 3");
        assert_eq!(limited.status, Status::Executed, "{}", limited.text);
        assert!(
            limited.text.starts_with("truncated: none (3 rows)\n"),
            "{}",
            limited.text
        );
        assert_eq!(row_lines(&limited.text), 3, "{}", limited.text);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_statement_that_needs_approval_waits_and_opens_nothing(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) =
            inject_approval(&workspace, cx, "Orders", "INSERT INTO t VALUES (9, 'z')");
        cx.run_until_parked();
        assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(rows_in_orders(), 3);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refused_statement_answers_the_reason_it_was_refused(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        for (connection, sql, reason) in [
            (
                "Orders",
                "DROP TABLE t",
                "DROP is refused for agent statements",
            ),
            ("Orders", "SELECT 1; SELECT 2", "more than one statement"),
            (
                "Secret",
                "SELECT 1",
                "no agent-visible connection has that name",
            ),
        ] {
            let answers = inject_sql(&workspace, cx, connection, sql);
            cx.run_until_parked();
            let reply = answers.try_recv().expect("the gate answers at once");
            assert_eq!(reply.status, Status::RefusedByGate, "{sql}");
            assert_eq!(reply.text, reason, "{sql}");
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
        });
        assert_eq!(rows_in_orders(), 3);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_empty_statement_is_refused_without_opening_a_connection(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        for sql in ["", "   "] {
            let answers = inject_sql(&workspace, cx, "Orders", sql);
            cx.run_until_parked();
            let reply = answers.try_recv().expect("the gate answers at once");
            assert_eq!(reply.status, Status::RefusedByGate, "{sql:?}");
            assert_eq!(reply.text, "there is no statement to run", "{sql:?}");
        }
        assert!(pending_calls(&workspace, cx).is_empty());
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.calls.is_empty(), "a call was held");
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
        });
        quit(workspace, cx);
    });
}

/// Whether `text` holds `name` with a character outside a name on each side.
fn names_whole(text: &str, name: &str) -> bool {
    let in_a_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(in_a_name)
            && !text[at + name.len()..]
                .chars()
                .next()
                .is_some_and(in_a_name)
    })
}

#[cfg(unix)]
#[gpui::test]
fn a_failed_statement_answers_the_servers_words_without_the_path(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let socket = crate::agent::socket_path().expect("the test home gives a socket path");
        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db")
            .display()
            .to_string();
        let ask = |cx: &mut VisualTestContext, sql: String| {
            let shim = crate::agent::shim::testing::call_through_the_shim(
                socket.clone(),
                "execute_sql",
                serde_json::json!({ "connection": "Orders", "sql": sql }),
            );
            drive_until(cx, |_| shim.is_finished());
            let result = shim.join().expect("the shim's thread ends");
            result["content"][0]["text"]
                .as_str()
                .expect("a result carries text")
                .to_string()
        };

        // The agent names the database file itself, so the server's message
        // repeats it.
        let failed = ask(cx, format!("SELECT * FROM \"{database}\""));
        assert!(failed.starts_with("status: failed"), "{failed}");
        assert!(failed.contains("no such table"), "{failed}");
        assert!(failed.contains("[hidden]"), "{failed}");
        assert!(!names_whole(&failed, &database), "{failed}");
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.agent.lanes.contains_key("orders"));
        });

        let fresh = ask(cx, "SELECT 1".to_string());
        assert!(fresh.starts_with("status: executed"), "{fresh}");

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_connection_that_will_not_open_answers_a_fixed_sentence(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        // A directory where the database file was: nothing can open it.
        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db");
        std::fs::remove_file(&database).expect("the database file is there");
        std::fs::create_dir(&database).expect("a directory takes its place");
        let answers = inject_sql(&workspace, cx, "Orders", "SELECT 1");
        let mut reply = None;
        drive_until(cx, |_| {
            reply = reply.take().or_else(|| answers.try_recv().ok());
            reply.is_some()
        });
        let reply = reply.expect("the call answered");
        assert_eq!(reply.status, Status::Failed);
        assert_eq!(reply.text, super::agent_read::OPEN_FAILED);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_failed_call_audits_the_servers_words_without_the_path(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db");
        let read = |cx: &mut VisualTestContext, sql: &str| {
            let answers = inject_sql(&workspace, cx, "Orders", sql);
            let mut reply = None;
            drive_until(cx, |_| {
                reply = reply.take().or_else(|| answers.try_recv().ok());
                reply.is_some()
            });
            reply.expect("the call answered")
        };

        // The statement names the file, so the server's words repeat it.
        let named = format!("SELECT * FROM \"{}\"", database.display());
        let failed = read(cx, &named);
        assert_eq!(failed.status, Status::Failed, "{}", failed.text);
        assert!(
            !names_whole(&failed.text, &database.display().to_string()),
            "{}",
            failed.text
        );
        let missing = read(cx, "SELECT * FROM missing");
        assert_eq!(missing.status, Status::Failed, "{}", missing.text);
        let log = agent_log(&workspace, cx);
        assert_eq!(log[0].sql.as_deref(), Some("SELECT * FROM missing"));
        let error = log[0].error.as_deref().expect("the failure is audited");
        assert!(error.contains("no such table: missing"), "{error}");
        let error = log[1].error.as_deref().expect("the failure is audited");
        assert!(error.contains("no such table"), "{error}");
        assert!(
            !names_whole(error, &database.display().to_string()),
            "the audit keeps the path: {error}"
        );

        // A directory where the database file was: nothing can open it.
        std::fs::remove_file(&database).expect("the database file is there");
        std::fs::create_dir(&database).expect("a directory takes its place");
        let reply = read(cx, "SELECT 1");
        assert_eq!(reply.status, Status::Failed);
        assert_eq!(reply.text, super::agent_read::OPEN_FAILED);
        let log = agent_log(&workspace, cx);
        let opened = log[0]
            .error
            .as_deref()
            .expect("the open failure is audited");
        assert!(!opened.is_empty());
        assert!(
            !names_whole(opened, &database.display().to_string()),
            "the audit keeps the path: {opened}"
        );
        for entry in &log {
            assert_eq!(entry.status, Status::Failed);
        }
        assert!(
            !reply.text.contains(opened),
            "the open error reached the agent: {}",
            reply.text
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_long_statement_is_clipped_in_the_audit_on_a_character_boundary(cx: &mut TestAppContext) {
    use super::agent::{AUDIT_SQL_CAP, SQL_CLIPPED};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let sql = format!("SELECT '{}'", "\u{2026}".repeat(5000));
        let answers = inject_sql(&workspace, cx, "Orders", &sql);
        let mut reply = None;
        drive_until(cx, |_| {
            reply = reply.take().or_else(|| answers.try_recv().ok());
            reply.is_some()
        });
        let log = agent_log(&workspace, cx);
        let kept = log[0].sql.as_deref().expect("the statement is audited");
        assert_eq!(
            kept.chars().count(),
            AUDIT_SQL_CAP + SQL_CLIPPED.chars().count()
        );
        assert!(kept.ends_with(SQL_CLIPPED));
        assert!(sql.starts_with(&kept[..kept.len() - SQL_CLIPPED.len()]));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_read_that_outlasts_the_agent_timeout_is_stopped(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "statement_timeout = 1");
        cx.executor().allow_parking();
        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db")
            .display()
            .to_string();
        // Real time: the connection's own timer interrupts the statement.
        let read = |cx: &mut VisualTestContext, sql: &str| {
            let answers = inject_sql(&workspace, cx, "Orders", sql);
            let mut reply = None;
            drive_until(cx, |_| {
                reply = reply.take().or_else(|| answers.try_recv().ok());
                reply.is_some()
            });
            reply.expect("the read answered")
        };

        let started = std::time::Instant::now();
        let runaway = read(
            cx,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
             SELECT count(*) FROM n",
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the runaway ran {:?}",
            started.elapsed()
        );
        assert_eq!(runaway.status, Status::Failed, "{}", runaway.text);
        assert!(!names_whole(&runaway.text, &database), "{}", runaway.text);
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.agent.lanes.contains_key("orders"));
        });

        let fresh = read(cx, "SELECT 1");
        assert_eq!(fresh.status, Status::Executed, "{}", fresh.text);
        quit(workspace, cx);
    });
}

/// What an injected read answered, waiting on the executor for it.
fn answer_of(
    cx: &mut VisualTestContext,
    answers: &std::sync::mpsc::Receiver<crate::agent::Reply>,
) -> crate::agent::Reply {
    let mut reply = None;
    drive_until(cx, |_| {
        reply = reply.take().or_else(|| answers.try_recv().ok());
        reply.is_some()
    });
    reply.expect("the call answered")
}

/// The statuses the log holds for `id`.
fn logged(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: crate::agent::CallId,
) -> Vec<crate::agent::Status> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .agent
            .log
            .iter()
            .filter(|entry| entry.id == id)
            .map(|entry| entry.status)
            .collect()
    })
}

#[gpui::test]
fn calls_on_one_profile_run_one_at_a_time_in_arrival_order(cx: &mut TestAppContext) {
    use crate::agent::{Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let sql = |sql: &str| crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            ..Default::default()
        };
        let (first, first_answers) = inject_call_with(
            &workspace,
            cx,
            Tool::ExecuteSql,
            Some("Orders"),
            sql("SELECT 1"),
        );
        let (second, second_answers) = inject_call_with(
            &workspace,
            cx,
            Tool::ExecuteSql,
            Some("Orders"),
            sql("SELECT 2"),
        );
        workspace.read_with(cx, |workspace, _| {
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert_eq!(lane.running.as_ref().map(|running| running.id), Some(first));
            assert_eq!(
                lane.waiting
                    .iter()
                    .map(|waiting| waiting.id)
                    .collect::<Vec<_>>(),
                [second]
            );
        });

        let one = answer_of(cx, &first_answers);
        let two = answer_of(cx, &second_answers);
        assert_eq!(one.status, Status::Executed, "{}", one.text);
        assert_eq!(two.status, Status::Executed, "{}", two.text);
        assert!(one.text.contains("[\"1\"]"), "{}", one.text);
        assert!(two.text.contains("[\"2\"]"), "{}", two.text);
        // Newest first: the second ended after the first.
        workspace.read_with(cx, |workspace, _| {
            let ended: Vec<_> = workspace.agent.log.iter().map(|entry| entry.id).collect();
            assert_eq!(ended, [second, first]);
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert!(lane.running.is_none() && lane.waiting.is_empty());
            assert!(lane.connection.is_some());
        });
        quit(workspace, cx);
    });
}

/// `agent_sqlite`'s two profiles with a DuckDB profile `Ducks`, visible to the
/// agent, on a file of its own. Answers the file's path.
fn agent_duckdb<'a>(
    cx: &'a mut TestAppContext,
    path: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    agent_duckdb_with(cx, path, "")
}

/// [`agent_duckdb`] with more scalar keys for `Ducks`, such as its mode.
fn agent_duckdb_with<'a>(
    cx: &'a mut TestAppContext,
    path: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_with_profiles(
        cx,
        &[
            sqlite_profile("secret", "Secret", ""),
            format!(
                "[[profiles]]\nid = \"ducks\"\nname = \"Ducks\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"duckdb\"\npath = {path:?}\nagent_visible = true\n{extra}\n"
            ),
        ]
        .concat(),
    )
}

#[gpui::test]
fn an_agent_read_on_a_duckdb_file_sees_the_latest_commit(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let path = directory.join("ducks.duckdb").display().to_string();
        // The user's own instance, held open for the whole test.
        let user = ::duckdb::Connection::open(&path).expect("the file opens");
        user.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)")
            .expect("the schema applies");
        let (workspace, cx) = agent_duckdb(cx, &path);
        cx.executor().allow_parking();

        let count = |cx: &mut VisualTestContext| {
            let answers = inject_sql(&workspace, cx, "Ducks", "SELECT count(*) FROM t");
            let answer = answer_of(cx, &answers);
            assert_eq!(answer.status, Status::Executed, "{}", answer.text);
            workspace.read_with(cx, |workspace, _| {
                let lane = workspace.agent.lanes.get("ducks");
                assert!(
                    lane.is_none_or(|lane| lane.connection.is_none()),
                    "a DuckDB read keeps no connection"
                );
            });
            answer.text
        };
        let first = count(cx);
        assert!(first.contains("[\"1\"]"), "{first}");
        user.execute_batch("INSERT INTO t VALUES (2)")
            .expect("the user commits a row");
        let second = count(cx);
        assert!(second.contains("[\"2\"]"), "{second}");
        drop(user);
        quit(workspace, cx);
    });
}

/// A DuckDB profile `Ducks` (Read-only) beside a CSV `rows.csv` of three rows,
/// both under the test home. Answers the workspace, the user's own instance of
/// the file (held open for the whole test) and the CSV's path.
#[cfg(unix)]
fn agent_duckdb_with_csv(
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    &mut VisualTestContext,
    ::duckdb::Connection,
    String,
) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let path = directory.join("ducks.duckdb").display().to_string();
    let csv = directory.join("rows.csv");
    std::fs::write(&csv, "id\n1\n2\n3\n").expect("the CSV is writable");
    let user = ::duckdb::Connection::open(&path).expect("the file opens");
    user.execute_batch("CREATE TABLE t (id INTEGER)")
        .expect("the schema applies");
    let (workspace, cx) = agent_duckdb(cx, &path);
    cx.executor().allow_parking();
    (workspace, cx, user, csv.display().to_string())
}

#[cfg(unix)]
#[gpui::test]
fn an_approved_read_of_a_file_runs_read_only_and_answers_rows(cx: &mut TestAppContext) {
    use crate::{agent::Status, sql::Mode};

    store::tests::with_home(|| {
        let (workspace, cx, user, csv) = agent_duckdb_with_csv(cx);
        let (id, answers) = inject_approval(
            &workspace,
            cx,
            "Ducks",
            &format!("SELECT count(*) AS n FROM '{csv}'"),
        );
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert_eq!(
            answers.try_recv().err(),
            Some(std::sync::mpsc::TryRecvError::Empty),
            "the read waits for the human"
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });

        approve_front(cx);
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::Executed, "{}", answer.text);
        assert!(
            answer
                .text
                .starts_with("truncated: none (1 row)\ncolumns: [{\"name\":\"n\""),
            "{}",
            answer.text
        );
        assert!(answer.text.contains("[\"3\"]"), "{}", answer.text);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty(), "a write was started");
            let entry = workspace.agent.log.front().expect("the call is logged");
            assert_eq!(entry.status, Status::Executed);
            assert_eq!(
                entry.verdict,
                Some(super::agent::AuditVerdict::Approve {
                    mode: Mode::ReadOnly,
                    expiry: None
                })
            );
        });
        let lines = audit_lines();
        let phase_of = |phase: &str| {
            lines
                .iter()
                .position(|line| line["id"] == id.0 && line["phase"] == phase)
                .unwrap_or_else(|| panic!("no {phase} line for the call: {lines:?}"))
        };
        let (started, outcome) = (phase_of("started"), phase_of("outcome"));
        assert!(started < outcome, "the start is recorded before the end");
        assert_eq!(lines[started]["decision"], "approved");
        assert_eq!(lines[outcome]["decision"], "approved");
        drop(user);
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_approved_read_that_cannot_be_recorded_does_not_run(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx, user, csv) = agent_duckdb_with_csv(cx);
        // A directory where the file belongs: nothing can be appended to it.
        let log = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join(store::AGENT_AUDIT_FILE);
        std::fs::create_dir_all(&log).expect("the obstacle is creatable");

        let (_, answers) = inject_approval(
            &workspace,
            cx,
            "Ducks",
            &format!("SELECT count(*) AS n FROM '{csv}'"),
        );
        approve_front(cx);
        assert_eq!(
            answer_of(cx, &answers),
            Reply {
                status: Status::Failed,
                text: super::agent::NOT_RECORDED.to_string(),
            }
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .agent
                    .lanes
                    .get("ducks")
                    .is_none_or(|lane| lane.running.is_none() && lane.waiting.is_empty()),
                "a read was handed to the lane"
            );
            assert!(workspace.agent.writes.is_empty(), "a write was started");
            let entry = workspace.agent.log.front().expect("the call is logged");
            assert_eq!(entry.status, Status::Failed);
        });
        drop(user);
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_read_shown_at_read_write_and_lowered_to_read_only_is_not_run(cx: &mut TestAppContext) {
    use crate::{
        agent::{Reply, Status},
        sql::Mode,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        // An unlisted function asks at Read-write and runs at Read-only, so
        // the lowered profile would give this statement another verdict.
        let (id, answers) = inject_approval(&workspace, cx, "Orders", "SELECT my_udf(1)");
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.active = Some(
                workspace
                    .profiles
                    .iter()
                    .position(|profile| profile.id == "orders")
                    .expect("Orders is in the list"),
            );
            workspace.set_mode(Mode::ReadOnly, cx);
        });
        approve_front(cx);
        assert_eq!(
            answer_of(cx, &answers),
            Reply {
                status: Status::RefusedByGate,
                text: super::agent::CHANGED_SINCE_SHOWN.to_string(),
            }
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a lane was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        assert!(
            !audit_lines()
                .iter()
                .any(|line| line["id"] == id.0 && line["phase"] == "started"),
            "a call that did not run was recorded as started"
        );
        quit(workspace, cx);
    });
}

/// What Approve answers when the call in front is armed, its pending
/// statement is made `pending` and the call is approved in the same update,
/// so no frame is drawn between the two.
fn approve_with_pending(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: crate::agent::CallId,
    pending: &str,
) {
    settle(cx);
    cx.executor().advance_clock(super::agent::ARM_DELAY);
    settle(cx);
    workspace.update(cx, |workspace, cx| {
        let call = workspace
            .agent
            .calls
            .iter_mut()
            .find(|call| call.id == id)
            .expect("the call is pending");
        call.approval.sql = pending.to_string();
        workspace.approve_call(id, cx);
    });
    settle(cx);
}

#[track_caller]
fn assert_cancelled_as_changed(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: crate::agent::CallId,
    answers: &std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    use crate::agent::{Reply, Status};

    assert_eq!(
        answer_of(cx, answers),
        Reply {
            status: Status::RefusedByGate,
            text: super::agent::CHANGED_SINCE_SHOWN.to_string(),
        }
    );
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.agent.writes.is_empty(), "a write was started");
        assert!(workspace.agent.lanes.is_empty(), "a lane was opened");
    });
    assert!(
        !audit_lines()
            .iter()
            .any(|line| line["id"] == id.0 && line["phase"] == "started"),
        "a call that did not run was recorded as started"
    );
}

#[gpui::test]
fn a_one_byte_difference_cancels_the_approval(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        approve_with_pending(
            &workspace,
            cx,
            id,
            "UPDATE t SET email = 'z@x' WHERE id = 2",
        );
        assert_cancelled_as_changed(&workspace, cx, id, &answers);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        assert_eq!(email_of(2), None);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_differently_encoded_statement_cancels_the_approval(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // Equality is byte for byte: no normalisation, folding or trimming.
        let precomposed = "UPDATE t SET email = '\u{e9}' WHERE id = 1";
        let decomposed = "UPDATE t SET email = 'e\u{301}' WHERE id = 1";
        let spaced = "UPDATE t SET email = 'z@x' WHERE id = 1 ";
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        for (drawn, pending) in [
            (precomposed, decomposed),
            (spaced, UPDATE_ONE),
            (UPDATE_ONE, spaced),
        ] {
            let (id, answers) = inject_approval(&workspace, cx, "Orders", drawn);
            approve_with_pending(&workspace, cx, id, pending);
            assert_cancelled_as_changed(&workspace, cx, id, &answers);
            assert_eq!(email_of(1).as_deref(), Some("a@x"), "{drawn:?}");
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approval_never_drawn_never_runs(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            // As if the dialog had never drawn this call.
            *workspace.approval_shown.borrow_mut() = None;
            workspace.approve_call(id, cx);
        });
        settle(cx);
        assert_cancelled_as_changed(&workspace, cx, id, &answers);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_lowered_mode_cancels_the_approval(cx: &mut TestAppContext) {
    use crate::sql::Mode;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.active = Some(
                workspace
                    .profiles
                    .iter()
                    .position(|profile| profile.id == "orders")
                    .expect("Orders is in the list"),
            );
            workspace.set_mode(Mode::ReadOnly, cx);
        });
        approve_front(cx);
        assert_cancelled_as_changed(&workspace, cx, id, &answers);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_changed_connection_cancels_the_approval(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        workspace.update(cx, |workspace, _| {
            let profile = workspace
                .profiles
                .iter_mut()
                .find(|profile| profile.id == "orders")
                .expect("Orders is in the list");
            profile.config = profile.config.clone().with_statement_timeout(7);
        });
        approve_front(cx);
        assert_cancelled_as_changed(&workspace, cx, id, &answers);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_approval_for_a_profile_hidden_at_approve_is_rejected(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        // Hidden on the field, without the sweep that would end the call.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace
                .profiles
                .iter_mut()
                .find(|profile| profile.id == "orders")
                .expect("Orders is in the list");
            profile.agent_visible = false;
            workspace.approve_call(id, cx);
        });
        settle(cx);
        assert_eq!(
            answer_of(cx, &answers),
            Reply {
                status: Status::Rejected,
                text: super::agent::NO_LONGER_AVAILABLE.to_string(),
            }
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.writes.is_empty(), "a write was started");
            assert!(workspace.agent.lanes.is_empty(), "a lane was opened");
        });
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_agent_write_on_duckdb_is_refused_before_it_is_shown(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let path = directory.join("ducks.duckdb").display().to_string();
        // The user's own instance, held open for the whole test.
        let user = ::duckdb::Connection::open(&path).expect("the file opens");
        user.execute_batch("CREATE TABLE t (id INTEGER)")
            .expect("the schema applies");
        let (workspace, cx) = agent_duckdb_with(cx, &path, "mode = \"read-write\"");
        cx.executor().allow_parking();

        let answers = inject_sql(&workspace, cx, "Ducks", "CREATE INDEX i ON t (id)");
        let answer = answer_of(cx, &answers);
        assert_eq!(
            answer,
            Reply {
                status: Status::RefusedByGate,
                text: crate::db::DUCKDB_WRITE_REFUSED.to_string(),
            }
        );
        settle(cx);
        assert_ne!(front_dialog(&workspace, cx), Some("agent-approval"));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.calls.is_empty(), "the write was held");
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        let indexes: i64 = user
            .query_row(
                "SELECT count(*) FROM duckdb_indexes() WHERE index_name = 'i'",
                [],
                |row| row.get(0),
            )
            .expect("the catalog reads");
        assert_eq!(indexes, 0);
        drop(user);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_agent_read_on_an_in_memory_duckdb_profile_is_refused_by_name(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_duckdb(cx, "");
        cx.executor().allow_parking();
        let answers = inject_sql(&workspace, cx, "Ducks", "SELECT 1");
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::RefusedByGate, "{}", answer.text);
        assert_eq!(
            answer.text,
            "This connection is an in-memory DuckDB database, which has no separate connection an agent can read."
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "nothing was opened");
            let entry = workspace.agent.log.front().expect("the refusal is logged");
            assert_eq!(entry.status, Status::RefusedByGate);
            assert_eq!(entry.resolved.as_deref(), Some("Ducks"));
            assert!(
                matches!(&entry.verdict, Some(crate::workspace::agent::AuditVerdict::Refuse(reason)) if reason.contains("in-memory DuckDB")),
                "{:?}",
                entry.verdict
            );
        });
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn an_in_memory_duckdb_profile_refuses_a_read_it_would_hold_before_asking_anyone(
    cx: &mut TestAppContext,
) {
    use crate::{
        agent::Status,
        sql::{AgentVerdict, Mode, agent_gate},
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_duckdb(cx, "");
        cx.executor().allow_parking();
        let sql = "SELECT * FROM 'data.csv'";
        assert!(
            matches!(
                agent_gate(Engine::DuckDb, sql, Mode::ReadOnly),
                AgentVerdict::Approve(..)
            ),
            "the gate holds a read of a file for a person"
        );
        let (id, answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Ducks"),
            crate::agent::CallArgs {
                sql: Some(sql.to_string()),
                ..Default::default()
            },
        );
        let answer = answer_of(cx, &answers);
        assert_eq!(answer.status, Status::RefusedByGate, "{}", answer.text);
        assert!(answer.text.contains("in-memory DuckDB"), "{}", answer.text);
        settle(cx);
        assert_ne!(front_dialog(&workspace, cx), Some("agent-approval"));
        assert!(
            pending_calls(&workspace, cx).is_empty(),
            "the read was held"
        );
        let lines = audit_of(id);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["phase"], "outcome");
        assert!(lines[0].get("decision").is_none(), "{lines:?}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn hanging_up_a_waiting_read_never_starts_it(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    use crate::agent::{Inbound, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let (first, first_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some("SELECT 1".to_string()),
                ..Default::default()
            },
        );
        let (second, second_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some("SELECT 2".to_string()),
                ..Default::default()
            },
        );
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id: second }, cx)
        });
        assert_eq!(
            second_answers.try_recv().err(),
            Some(TryRecvError::Disconnected),
            "nothing is sent to a caller that hung up"
        );
        workspace.read_with(cx, |workspace, _| {
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert!(lane.waiting.is_empty());
            assert_eq!(lane.running.as_ref().map(|running| running.id), Some(first));
        });

        let one = answer_of(cx, &first_answers);
        assert_eq!(one.status, Status::Executed, "{}", one.text);
        cx.run_until_parked();
        assert_eq!(logged(&workspace, cx, second), [Status::Cancelled]);
        assert_eq!(logged(&workspace, cx, first), [Status::Executed]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn hanging_up_a_running_read_cancels_it_and_frees_the_lane(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    use crate::agent::{Inbound, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let (first, first_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some("SELECT 1".to_string()),
                ..Default::default()
            },
        );
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id: first }, cx)
        });
        assert_eq!(logged(&workspace, cx, first), [Status::Cancelled]);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.agent.lanes.get("orders").is_none_or(|lane| {
                    lane.running.is_none() && lane.waiting.is_empty() && lane.connection.is_none()
                }),
                "the read's lane still holds it"
            );
        });

        // The read's late result changes nothing.
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(50));
        cx.run_until_parked();
        assert_eq!(
            first_answers.try_recv().err(),
            Some(TryRecvError::Disconnected)
        );
        assert_eq!(logged(&workspace, cx, first), [Status::Cancelled]);

        let (_, next_answers) = inject_call_with(
            &workspace,
            cx,
            crate::agent::tools::Tool::ExecuteSql,
            Some("Orders"),
            crate::agent::CallArgs {
                sql: Some("SELECT 3".to_string()),
                ..Default::default()
            },
        );
        let next = answer_of(cx, &next_answers);
        assert_eq!(next.status, Status::Executed, "{}", next.text);
        assert!(next.text.contains("[\"3\"]"), "{}", next.text);
        quit(workspace, cx);
    });
}

/// Whether the read running on `profile_id`'s lane holds the connection a
/// cancel would be sent to, stepping the scheduler one task at a time until it
/// does or the read has answered. The test scheduler runs a statement inline,
/// so a statement that is still running cannot be waited on here; what can be
/// shown is that the connection is reachable before the statement is sent.
fn connection_is_held_before_the_statement_runs(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    profile_id: &str,
    answers: &std::sync::mpsc::Receiver<crate::agent::Reply>,
) -> bool {
    loop {
        let held = workspace.read_with(cx, |workspace, _| {
            workspace
                .agent
                .lanes
                .get(profile_id)
                .and_then(|lane| lane.running.as_ref())
                .is_some_and(|running| running.holds_connection())
        });
        if held {
            return true;
        }
        if answers.try_recv().is_ok() || !cx.executor().tick() {
            return false;
        }
    }
}

#[gpui::test]
fn the_first_read_of_a_lane_can_be_cancelled_on_the_connection_it_opened(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let (_, answers) = inject_orders_read(&workspace, cx, "SELECT 1");
        workspace.read_with(cx, |workspace, _| {
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert!(lane.connection.is_none(), "the lane starts cold");
        });
        assert!(
            connection_is_held_before_the_statement_runs(&workspace, cx, "orders", &answers),
            "a hangup, the deadline, a hide or the kill switch would have no connection to cancel on"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_duckdb_read_can_be_cancelled_on_the_connection_it_opened(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let path = directory.join("ducks.duckdb").display().to_string();
        ::duckdb::Connection::open(&path)
            .and_then(|connection| connection.execute_batch("CREATE TABLE t (id INTEGER)"))
            .expect("the schema applies");
        let (workspace, cx) = agent_duckdb(cx, &path);
        cx.executor().allow_parking();
        let answers = inject_sql(&workspace, cx, "Ducks", "SELECT count(*) FROM t");
        assert!(
            connection_is_held_before_the_statement_runs(&workspace, cx, "ducks", &answers),
            "a DuckDB lane keeps no connection, so only the running call can hand one to a cancel"
        );
        quit(workspace, cx);
    });
}

/// How many times, and as which backend, [`open_live_postgres`] opened.
static LIVE_OPENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LIVE_BACKEND: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The compose Postgres from the `PG*` variables, as the live tests reach it.
fn live_postgres() -> crate::db::ConnectionConfig {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    crate::db::ConnectionConfig::postgres(crate::db::ServerConfig {
        host: var("PGHOST"),
        port: std::env::var("PGPORT")
            .ok()
            .map(|port| port.parse().expect("PGPORT must be a number")),
        database: var("PGDATABASE"),
        user: var("PGUSER"),
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
        sslmode: crate::db::SslMode::Disable,
        ..crate::db::ServerConfig::default()
    })
}

/// An [`agent_read::Opener`](super::agent_read::Opener) that opens the live
/// Postgres whatever profile asked, and remembers which backend it got.
fn open_live_postgres(
    executor: &gpui::BackgroundExecutor,
    _: crate::db::ConnectionConfig,
    _: String,
) -> gpui::Task<Result<(crate::db::Connection, Vec<String>), crate::db::DbError>> {
    use std::sync::atomic::Ordering;

    super::spawn_blocking(executor, || {
        let connection = crate::db::Connection::open_for_agent(live_postgres())?;
        let backend = connection.query(
            "SELECT pg_backend_pid()",
            &crate::db::CancelToken::default(),
            crate::db::Fetch::default(),
        )?;
        let backend = backend.rows[0].get(0).unwrap_or_default().parse();
        LIVE_BACKEND.store(backend.unwrap_or_default(), Ordering::SeqCst);
        LIVE_OPENS.fetch_add(1, Ordering::SeqCst);
        Ok((connection, Vec::new()))
    })
}

#[gpui::test]
#[ignore = "requires the compose databases"]
fn live_a_read_reopens_the_session_a_server_dropped_while_the_lane_was_idle(
    cx: &mut TestAppContext,
) {
    use std::sync::atomic::Ordering;

    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, _| {
            workspace.agent.opener = Some(open_live_postgres);
        });
        LIVE_OPENS.store(0, Ordering::SeqCst);

        let (_, answers) = inject_orders_read(&workspace, cx, "SELECT 1");
        let first = answer_of(cx, &answers);
        assert_eq!(first.status, Status::Executed, "{}", first.text);
        assert_eq!(LIVE_OPENS.load(Ordering::SeqCst), 1);

        // The server ends the session while nobody is using it.
        let other = crate::db::Connection::open(live_postgres()).expect("a second session opens");
        other
            .query(
                &format!(
                    "SELECT pg_terminate_backend({})",
                    LIVE_BACKEND.load(Ordering::SeqCst)
                ),
                &crate::db::CancelToken::default(),
                crate::db::Fetch::default(),
            )
            .expect("the backend is terminated");

        let (_, answers) = inject_orders_read(&workspace, cx, "SELECT 2");
        let second = answer_of(cx, &answers);
        assert_eq!(second.status, Status::Executed, "{}", second.text);
        assert!(second.text.contains("[\"2\"]"), "{}", second.text);
        assert_eq!(LIVE_OPENS.load(Ordering::SeqCst), 2, "one reopen");
        quit(workspace, cx);
    });
}

/// An `execute_sql` call on `Orders` carrying `sql`, injected, with its id.
fn inject_orders_read(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    sql: &str,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExecuteSql,
        Some("Orders"),
        crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            ..Default::default()
        },
    )
}

#[gpui::test]
fn a_hung_open_ends_its_call_at_the_deadline_and_frees_the_lane(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use super::agent_read::CALL_DEADLINE;
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, _| {
            workspace.agent.opener = Some(|executor, _, _| executor.spawn(std::future::pending()));
        });
        let (first, first_answers) = inject_orders_read(&workspace, cx, "SELECT 1");
        let (_, second_answers) = inject_orders_read(&workspace, cx, "SELECT 2");

        cx.executor()
            .advance_clock(CALL_DEADLINE - Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(first_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(second_answers.try_recv().err(), Some(TryRecvError::Empty));

        workspace.update(cx, |workspace, _| workspace.agent.opener = None);
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        let one = answer_of(cx, &first_answers);
        assert_eq!(one.status, Status::Failed);
        assert_eq!(one.text, "The call did not finish within 60 seconds.");
        assert_eq!(logged(&workspace, cx, first), [Status::Failed]);

        let two = answer_of(cx, &second_answers);
        assert_eq!(two.status, Status::Executed, "{}", two.text);
        assert!(two.text.contains("[\"2\"]"), "{}", two.text);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_deadline_and_a_hangup_on_the_same_call_end_it_once(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use super::agent_read::CALL_DEADLINE;
    use crate::agent::{Inbound, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, _| {
            workspace.agent.opener = Some(|executor, _, _| executor.spawn(std::future::pending()));
        });
        let (first, first_answers) = inject_orders_read(&workspace, cx, "SELECT 1");
        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id: first }, cx)
        });
        cx.executor()
            .advance_clock(CALL_DEADLINE + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(logged(&workspace, cx, first), [Status::Cancelled]);
        assert_eq!(
            first_answers.try_recv().err(),
            Some(TryRecvError::Disconnected),
            "nothing is sent after the hangup"
        );
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_running_read_is_not_a_waiting_approval(cx: &mut TestAppContext) {
    use crate::{agent::Status, ui::AgentMark};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "");
        cx.executor().allow_parking();
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));

        let answers = inject_sql(&workspace, cx, "Orders", "SELECT 1");
        workspace.read_with(cx, |workspace, _| {
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert!(lane.running.is_some(), "the read is running");
            assert!(workspace.front_approval().is_none());
            assert!(workspace.agent.calls.is_empty());
        });
        assert_eq!(
            agent_mark_state(&workspace, cx),
            Some(AgentMark::Connected { waiting: 0 })
        );
        // A frame with the read still running: no dialog goes up for it.
        cx.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(front_dialog(&workspace, cx), None);

        let reply = answer_of(cx, &answers);
        assert_eq!(reply.status, Status::Executed, "{}", reply.text);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

/// An `execute_sql` call on `connection`, injected, with its id.
fn inject_read(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection: &str,
    sql: &str,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExecuteSql,
        Some(connection),
        crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            ..Default::default()
        },
    )
}

/// The app launched on visible `Orders` and `Billing` over one database that
/// `schema` has been run against. Neither is connected for the user.
fn orders_and_billing<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let visible = "agent_visible = true";
    let (workspace, cx) = launch_with_profiles(
        cx,
        &[
            sqlite_profile("secret", "Secret", ""),
            sqlite_profile("orders", "Orders", visible),
            sqlite_profile("billing", "Billing", visible),
        ]
        .concat(),
    );
    rusqlite::Connection::open(
        store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db"),
    )
    .and_then(|connection| connection.execute_batch(schema))
    .expect("the schema must apply");
    (workspace, cx)
}

fn no_lane_for(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str) -> bool {
    workspace.read_with(cx, |workspace, _| !workspace.agent.lanes.contains_key(id))
}

#[gpui::test]
fn hiding_or_removing_a_profile_ends_its_reads_and_drops_its_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = orders_and_billing(cx, ORDERS_SCHEMA);
        cx.executor().allow_parking();
        let (running, running_answers) = inject_read(&workspace, cx, "Orders", "SELECT 1");
        let (waiting, waiting_answers) = inject_read(&workspace, cx, "Orders", "SELECT 2");
        let (_, billing_answers) = inject_read(&workspace, cx, "Billing", "SELECT 3");

        workspace.update(cx, |workspace, cx| {
            workspace.set_agent_visible("orders", false, cx)
        });
        assert_eq!(running_answers.try_recv().ok(), Some(swept()));
        assert_eq!(waiting_answers.try_recv().ok(), Some(swept()));
        assert!(no_lane_for(&workspace, cx, "orders"));
        assert_eq!(
            logged(&workspace, cx, running),
            [crate::agent::Status::Rejected]
        );
        assert_eq!(
            logged(&workspace, cx, waiting),
            [crate::agent::Status::Rejected]
        );
        // Billing is another profile's lane, and it answers as it would have.
        let billing = answer_of(cx, &billing_answers);
        assert_eq!(
            billing.status,
            crate::agent::Status::Executed,
            "{}",
            billing.text
        );
        assert!(running_answers.try_recv().is_err(), "answered twice");
        cx.run_until_parked();
        assert!(no_lane_for(&workspace, cx, "orders"));

        // Removal reaches a lane that has its connection, too.
        let (first, first_answers) = inject_read(&workspace, cx, "Billing", "SELECT 4");
        let (second, second_answers) = inject_read(&workspace, cx, "Billing", "SELECT 5");
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "billing")
                .expect("Billing is in the list");
            workspace.remove_profile(index, cx)
        });
        assert_eq!(first_answers.try_recv().ok(), Some(swept()));
        assert_eq!(second_answers.try_recv().ok(), Some(swept()));
        assert!(no_lane_for(&workspace, cx, "billing"));
        assert_eq!(logged(&workspace, cx, first).len(), 1);
        assert_eq!(logged(&workspace, cx, second).len(), 1);
        cx.run_until_parked();
        assert!(first_answers.try_recv().is_err(), "answered twice");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_hide_adopted_from_another_window_ends_the_running_read(cx: &mut TestAppContext) {
    use crate::agent::tools::Tool;

    store::tests::with_home(|| {
        let (workspace, cx) = orders_and_billing(cx, ORDERS_SCHEMA);
        cx.executor().allow_parking();
        let (id, answers) = inject_read(&workspace, cx, "Orders", "SELECT 1");
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .agent
                    .lanes
                    .get("orders")
                    .is_some_and(|lane| lane.running.is_some())
            );
        });

        // Another window hides Orders: only the file says so.
        let file = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("profiles.toml");
        let text = std::fs::read_to_string(&file).expect("profiles.toml reads");
        let text = text.replacen("agent_visible = true\n", "", 1);
        std::fs::write(&file, text).expect("profiles.toml is written");
        assert!(!saved_visible("orders"));

        let (_, listed) = inject_call(&workspace, cx, Tool::ListConnections, None);
        assert!(listed.try_recv().is_ok());
        assert_eq!(answers.try_recv().ok(), Some(swept()));
        assert!(no_lane_for(&workspace, cx, "orders"));
        assert_eq!(logged(&workspace, cx, id), [crate::agent::Status::Rejected]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_read_after_a_hide_saved_by_another_window_is_refused_at_once(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = orders_and_billing(cx, ORDERS_SCHEMA);
        cx.executor().allow_parking();
        let (_, first) = inject_read(&workspace, cx, "Orders", "SELECT 1");
        assert_eq!(answer_of(cx, &first).status, Status::Executed);

        // Another window hides Orders: only the file says so.
        let file = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("profiles.toml");
        let text = std::fs::read_to_string(&file).expect("profiles.toml reads");
        std::fs::write(&file, text.replacen("agent_visible = true\n", "", 1))
            .expect("profiles.toml is written");

        // The file changed, so the very next read takes the hide on.
        let (_, second) = inject_read(&workspace, cx, "Orders", "SELECT 2");
        let second = answer_of(cx, &second);
        assert_eq!(second.status, Status::RefusedByGate, "{}", second.text);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_kill_switch_ends_every_running_read(cx: &mut TestAppContext) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = orders_and_billing(cx, ORDERS_SCHEMA);
        cx.executor().allow_parking();
        let (_, orders) = inject_read(&workspace, cx, "Orders", "SELECT 1");
        let (_, waiting) = inject_read(&workspace, cx, "Orders", "SELECT 2");
        let (_, billing) = inject_read(&workspace, cx, "Billing", "SELECT 3");

        workspace.update(cx, |workspace, cx| workspace.disconnect_agents(cx));
        let turned_off = Reply {
            status: Status::Rejected,
            text: "Agent access was turned off in DBDelve.".to_string(),
        };
        for answers in [&orders, &waiting, &billing] {
            assert_eq!(answers.try_recv().ok(), Some(turned_off.clone()));
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty());
        });
        cx.run_until_parked();
        for answers in [&orders, &waiting, &billing] {
            assert!(answers.try_recv().is_err(), "answered twice");
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_profile_whose_connection_changed_is_reopened_for_the_next_read(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');",
            "",
        );
        cx.executor().allow_parking();
        let other = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("other.db");
        rusqlite::Connection::open(&other)
            .and_then(|connection| {
                connection.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('two');")
            })
            .expect("the second file is written");

        let (_, answers) = inject_read(&workspace, cx, "Orders", "SELECT v FROM t");
        let first = answer_of(cx, &answers);
        assert!(first.text.contains("[\"one\"]"), "{}", first.text);

        // The connection changes in place, with no hide.
        workspace.update(cx, |workspace, _| {
            let orders = workspace
                .profiles
                .iter_mut()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            let crate::db::ConnectionConfig::Sqlite { path, .. } = &mut orders.config else {
                panic!("Orders is a SQLite profile");
            };
            *path = other.display().to_string();
        });
        let (_, answers) = inject_read(&workspace, cx, "Orders", "SELECT v FROM t");
        let second = answer_of(cx, &answers);
        assert!(second.text.contains("[\"two\"]"), "{}", second.text);
        workspace.read_with(cx, |workspace, _| {
            let orders = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "orders")
                .expect("the profile was restored");
            let lane = workspace.agent.lanes.get("orders").expect("a lane");
            assert!(lane.opened_from == orders.config);
            assert!(lane.connection.is_some());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_agent_read_leaves_the_users_open_transaction_alone(cx: &mut TestAppContext) {
    use crate::session::ProfileState;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "PRAGMA journal_mode=wal; CREATE TABLE t (id INTEGER); \
             INSERT INTO t VALUES (1), (2);",
            "mode = \"read-write\"",
        );
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "orders")
                .expect("Orders is in the list");
            workspace.activate(index, cx);
        });
        drive_until(cx, |cx| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .is_some_and(|profile| matches!(profile.state, ProfileState::Connected(_)))
            })
        });

        let run = |cx: &mut VisualTestContext, sql: &str| {
            cx.simulate_keystrokes("secondary-a");
            cx.simulate_input(sql);
            cx.simulate_keystrokes("secondary-enter");
            settle(cx);
        };
        run(cx, "BEGIN");
        run(cx, "INSERT INTO t VALUES (3)");
        let user = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let profile = workspace.profile().expect("Orders is in front");
                (
                    profile.in_transaction,
                    profile.generation,
                    matches!(profile.state, ProfileState::Connected(_)),
                )
            })
        };
        let before = user(cx);
        assert!(before.0, "the user's transaction is open");

        let count = |cx: &mut VisualTestContext| {
            let (_, answers) = inject_read(&workspace, cx, "Orders", "SELECT count(*) FROM t");
            answer_of(cx, &answers)
        };
        let seen = count(cx);
        assert_eq!(seen.status, crate::agent::Status::Executed, "{}", seen.text);
        assert!(
            seen.text.contains("[\"2\"]"),
            "only committed rows: {}",
            seen.text
        );
        assert_eq!(
            user(cx),
            before,
            "the agent's read moved the user's session"
        );

        run(cx, "COMMIT");
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { .. } => {}
            QueryState::Failed(error) => panic!("the commit failed: {error}"),
            _ => panic!("the commit never completed"),
        });
        assert!(!user(cx).0, "the commit closed the transaction");
        let after = count(cx);
        assert!(after.text.contains("[\"3\"]"), "{}", after.text);
        quit(workspace, cx);
    });
}

const EXPLAINED_SCHEMA: &str = "CREATE TABLE a (id INTEGER PRIMARY KEY);\
     CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, x INTEGER);\
     CREATE INDEX b_x ON b (x);";

const EXPLAINED_QUERY: &str = "SELECT * FROM a JOIN b ON a.id = b.a_id WHERE b.x = 1";

/// An `explain_query` call for `Orders`, injected, and its answer.
fn explain_call(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    sql: &str,
    analyze: Option<bool>,
) -> crate::agent::Reply {
    let (_, answers) = inject_call_with(
        workspace,
        cx,
        crate::agent::tools::Tool::ExplainQuery,
        Some("Orders"),
        crate::agent::CallArgs {
            sql: Some(sql.to_string()),
            analyze,
            ..Default::default()
        },
    );
    answer_of(cx, &answers)
}

#[cfg(unix)]
#[gpui::test]
fn an_agent_explains_a_query_and_reads_findings_first(cx: &mut TestAppContext) {
    use crate::{
        agent::{Status, tools::Tool},
        workspace::agent::AuditVerdict,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, EXPLAINED_SCHEMA, "");
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");
        let shim = crate::agent::shim::testing::call_through_the_shim(
            path,
            "explain_query",
            serde_json::json!({"connection": "Orders", "sql": EXPLAINED_QUERY}),
        );
        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a result carries text");
        let body = text
            .strip_prefix("status: executed\n")
            .unwrap_or_else(|| panic!("not executed: {text}"));
        let lines: Vec<&str> = body.lines().collect();
        let at = |start: &str| {
            lines
                .iter()
                .position(|line| line.starts_with(start))
                .unwrap_or_else(|| panic!("no `{start}` line in {text}"))
        };
        let (truncated, plan, findings, tree) = (
            at("truncated:"),
            at("plan: engine=sqlite analyzed=no"),
            at("findings"),
            at("tree:"),
        );
        assert!(
            truncated < plan && plan < findings && findings < tree,
            "{text}"
        );
        assert!(
            lines[tree..].iter().any(|line| line.contains("n0")),
            "{text}"
        );

        let log = agent_log(&workspace, cx);
        assert_eq!(
            log.iter()
                .map(|entry| (entry.tool, entry.status))
                .collect::<Vec<_>>(),
            [(Tool::ExplainQuery, Status::Executed)]
        );
        let sql = log[0]
            .sql
            .as_deref()
            .expect("the audit holds the statement");
        assert!(sql.starts_with("EXPLAIN QUERY PLAN SELECT"), "{sql}");
        assert_eq!(log[0].verdict, Some(AuditVerdict::Run));
        assert!(log[0].error.is_none(), "{:?}", log[0]);
        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_plain_plan_of_an_update_changes_nothing(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(
            cx,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\
             INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');",
            "mode = \"read-write\"",
        );
        let values = || -> Vec<String> {
            let database = store::dbdelve_directory()
                .expect("the test home has a data directory")
                .join("e2e.db");
            let connection = rusqlite::Connection::open(database).expect("the database opens");
            let mut statement = connection
                .prepare("SELECT v FROM t ORDER BY id")
                .expect("the table is readable");
            statement
                .query_map([], |row| row.get(0))
                .expect("the rows read")
                .collect::<Result<_, _>>()
                .expect("every value is text")
        };
        let before = values();
        let reply = explain_call(&workspace, cx, "UPDATE t SET v = 'changed'", None);
        assert_eq!(reply.status, Status::Executed, "{}", reply.text);
        assert!(reply.text.contains("tree:"), "{}", reply.text);
        assert_eq!(values(), before, "a plain plan changed the table");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_omitted_analyze_explains_exactly_as_false(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, EXPLAINED_SCHEMA, "");
        let omitted = explain_call(&workspace, cx, EXPLAINED_QUERY, None);
        let asked_false = explain_call(&workspace, cx, EXPLAINED_QUERY, Some(false));
        assert_eq!(
            omitted.status,
            crate::agent::Status::Executed,
            "{}",
            omitted.text
        );
        assert_eq!(omitted.status, asked_false.status);
        assert_eq!(omitted.text, asked_false.text);
        let log = agent_log(&workspace, cx);
        assert_eq!(log.len(), 2);
        assert!(log[0].sql.is_some());
        assert_eq!(log[0].sql, log[1].sql);
        quit(workspace, cx);
    });
}

/// Whether `text` holds `name` as a whole token in any ASCII case: a token
/// character is one of `[A-Za-z0-9_]`, and `name` is not part of a longer run of
/// them. Written here rather than borrowed from the scrub so the test does not
/// trust the code it checks.
fn names_whole_any_case(text: &str, name: &str) -> bool {
    let in_a_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let (text, name) = (text.to_ascii_lowercase(), name.to_ascii_lowercase());
    text.match_indices(&name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(in_a_name)
            && !text[at + name.len()..]
                .chars()
                .next()
                .is_some_and(in_a_name)
    })
}

/// `profiles.toml` as another window would leave it with profile `id` saved
/// as agent-visible or not.
fn save_visibility_as_another_window(id: &str, visible: bool) {
    let file = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("profiles.toml");
    let mut saved: toml::Table = std::fs::read_to_string(&file)
        .expect("profiles.toml reads")
        .parse()
        .expect("profiles.toml parses");
    let profile = saved["profiles"]
        .as_array_mut()
        .expect("profiles are an array")
        .iter_mut()
        .find(|profile| profile["id"].as_str() == Some(id))
        .and_then(toml::Value::as_table_mut)
        .expect("the profile is saved");
    if visible {
        profile.insert("agent_visible".into(), true.into());
    } else {
        profile.remove("agent_visible");
    }
    std::fs::write(
        &file,
        toml::to_string(&saved).expect("profiles.toml serializes"),
    )
    .expect("profiles.toml is written");
}

#[cfg(unix)]
#[gpui::test]
fn list_connections_names_each_engine_by_name_and_engine_only(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let fixture = include_str!("../testdata/every-engine.toml");
        let configs = super::restored(fixture);
        let mut stored: toml::Table = fixture.parse().expect("the fixture parses");
        let mut profiles = stored
            .remove("profiles")
            .and_then(|profiles| match profiles {
                toml::Value::Array(profiles) => Some(profiles),
                _ => None,
            })
            .expect("the fixture holds profiles");
        // What the agent should be told about each profile, taken from the
        // fixture and the config the window restores from it.
        let mut expected = Vec::new();
        let mut identifying = Vec::new();
        for profile in &mut profiles {
            let table = profile.as_table_mut().expect("a profile is a table");
            table.insert("agent_visible".into(), true.into());
            let id = table["id"].as_str().expect("a profile has an id");
            let config = &configs
                .iter()
                .find(|(restored, _)| restored == id)
                .expect("every profile restores")
                .1;
            identifying.extend(config.identifying_text());
            expected.push(serde_json::json!({
                "name": table["name"].as_str().expect("a profile has a name"),
                "engine": config.engine().as_str(),
            }));
        }
        assert!(expected.len() >= 13, "the fixture shrank: {expected:?}");
        assert!(
            identifying.iter().any(|text| text == "sa"),
            "SQL Server's two-letter user is among what is scanned for"
        );
        let only_profiles = toml::Table::from_iter([("profiles".to_string(), profiles.into())]);
        let (workspace, cx) = launch_with_profiles(
            cx,
            &toml::to_string(&only_profiles).expect("the profiles serialize"),
        );
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let socket = crate::agent::socket_path().expect("the test home gives a socket path");

        let shim = crate::agent::shim::testing::call_through_the_shim(
            socket,
            "list_connections",
            serde_json::json!({}),
        );
        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a result carries text");
        let body = text
            .strip_prefix(&format!("status: {}\n", Status::Executed.as_str()))
            .unwrap_or_else(|| panic!("not executed: {text}"));

        let listed: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(body).expect("the body is an array of objects");
        assert_eq!(listed.len(), expected.len(), "{body}");
        for (entry, wanted) in listed.iter().zip(&expected) {
            let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["engine", "name"], "{entry:?}");
            assert_eq!(&serde_json::Value::Object(entry.clone()), wanted);
        }

        // With the values the agent is meant to get taken out, nothing about
        // where a database is, or how to get in, is left to find.
        let mut left = body.to_string();
        for wanted in &expected {
            for key in ["name", "engine"] {
                let value = serde_json::to_string(&wanted[key]).expect("a string serializes");
                left = left.replacen(&value, "", 1);
            }
        }
        assert_eq!(
            left,
            format!(
                "[{}]",
                vec![r#"{"name":,"engine":}"#; expected.len()].join(",")
            )
        );
        for text in &identifying {
            assert!(!names_whole_any_case(&left, text), "{text:?} in {left}");
        }

        workspace.update(cx, |workspace, _| workspace.agent.bridge.take());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn list_connections_reads_empty_byte_exact_and_after_a_hide_elsewhere(cx: &mut TestAppContext) {
    use crate::agent::{CallArgs, Status, tools::Tool};

    store::tests::with_home(|| {
        let name = "Ünï \"q\"";
        let (workspace, cx) = launch_with_profiles(cx, &sqlite_profile("odd", name, ""));
        cx.executor().allow_parking();
        let listed = |cx: &mut VisualTestContext| {
            let (_, answers) = inject_call(&workspace, cx, Tool::ListConnections, None);
            answer_of(cx, &answers)
        };

        let none = listed(cx);
        assert_eq!((none.status, none.text.as_str()), (Status::Executed, "[]"));

        // Another window shows it.
        save_visibility_as_another_window("odd", true);
        let shown = listed(cx);
        assert_eq!(shown.status, Status::Executed);
        assert_eq!(shown.text, r#"[{"name":"Ünï \"q\"","engine":"sqlite"}]"#);

        let schemas = |cx: &mut VisualTestContext, asked: &str| {
            let (_, answers) = inject_call_with(
                &workspace,
                cx,
                Tool::ListSchemas,
                Some(asked),
                CallArgs::default(),
            );
            answer_of(cx, &answers)
        };
        let found = schemas(cx, name);
        assert_eq!(found.status, Status::Executed, "{}", found.text);
        for spelling in [
            "ünï \"q\"",
            "ÜNÏ \"Q\"",
            " Ünï \"q\"",
            "Ünï \"q\" ",
            // The same letters, composed the other way.
            "U\u{308}ni\u{308} \"q\"",
        ] {
            let refused = schemas(cx, spelling);
            assert_eq!(refused.status, Status::RefusedByGate, "{spelling:?}");
            assert_eq!(
                refused.text, "no agent-visible connection has that name",
                "{spelling:?}"
            );
        }

        // Another window hides it again before the next call.
        save_visibility_as_another_window("odd", false);
        let hidden = listed(cx);
        assert_eq!(
            (hidden.status, hidden.text.as_str()),
            (Status::Executed, "[]")
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.profiles[0].agent_visible);
        });
        quit(workspace, cx);
    });
}

const MOVED_SCHEMA: &str = "CREATE TABLE a (id INTEGER PRIMARY KEY, email TEXT NOT NULL);\
     CREATE UNIQUE INDEX a_email ON a (email);\
     CREATE VIEW v AS SELECT id FROM a;\
     INSERT INTO a VALUES (1, 'x@y'), (2, 'z@y');";

#[gpui::test]
fn no_reply_dbdelve_writes_depends_on_where_the_file_is(cx: &mut TestAppContext) {
    use crate::agent::{CallArgs, Reply, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, MOVED_SCHEMA, "");
        cx.executor().allow_parking();
        let home = store::dbdelve_directory().expect("the test home has a data directory");
        let first = home.join("e2e.db");
        // Another directory with another name, holding the same bytes.
        let elsewhere = home.join("moved-away-from-here");
        std::fs::create_dir(&elsewhere).expect("the directory is made");
        let second = elsewhere.join("orders-copy.sqlite");
        std::fs::copy(&first, &second).expect("the database is copied");

        // The agent names the file in one statement, so the server's own
        // message repeats it.
        let calls = |path: &str| -> Vec<(Tool, CallArgs)> {
            let args = |schema: Option<&str>, table: Option<&str>, sql: Option<&str>| CallArgs {
                schema: schema.map(str::to_string),
                table: table.map(str::to_string),
                sql: sql.map(str::to_string),
                ..CallArgs::default()
            };
            vec![
                (Tool::ListConnections, args(None, None, None)),
                (Tool::ListSchemas, args(None, None, None)),
                (Tool::ListTables, args(Some("main"), None, None)),
                (Tool::ListTables, args(Some("nope"), None, None)),
                (Tool::DescribeTable, args(Some("main"), Some("a"), None)),
                (Tool::DescribeTable, args(Some("main"), Some("nope"), None)),
                (
                    Tool::ExecuteSql,
                    args(None, None, Some("SELECT id, email FROM a ORDER BY id")),
                ),
                (
                    Tool::ExecuteSql,
                    args(None, None, Some(&format!("SELECT * FROM \"{path}\""))),
                ),
                (
                    Tool::ExplainQuery,
                    args(None, None, Some("SELECT * FROM a WHERE email = 'x'")),
                ),
            ]
        };
        let mut ask = |path: &std::path::Path| -> Vec<Reply> {
            workspace.update(cx, |workspace, _| {
                let orders = workspace
                    .profiles
                    .iter_mut()
                    .find(|profile| profile.id == "orders")
                    .expect("the profile was restored");
                let crate::db::ConnectionConfig::Sqlite { path: stored, .. } = &mut orders.config
                else {
                    panic!("Orders is a SQLite profile");
                };
                *stored = path.display().to_string();
            });
            calls(&path.display().to_string())
                .into_iter()
                .map(|(tool, args)| {
                    let connection = (tool != Tool::ListConnections).then_some("Orders");
                    let (_, answers) = inject_call_with(&workspace, cx, tool, connection, args);
                    answer_of(cx, &answers)
                })
                .collect()
        };
        let there = ask(&first);
        let here = ask(&second);

        assert_eq!(there.len(), here.len());
        let stored_and_expanded = |path: &std::path::Path| {
            let stored = path.display().to_string();
            let expanded = store::home_expanded(&stored).display().to_string();
            [stored, expanded]
        };
        let paths: Vec<String> = [&first, &second]
            .into_iter()
            .flat_map(|path| stored_and_expanded(path))
            .collect();
        for (at, (a, b)) in there.iter().zip(&here).enumerate() {
            assert_eq!(a, b, "call {at} answered differently for the moved file");
            for path in &paths {
                assert!(
                    !names_whole_any_case(&a.text, path),
                    "call {at} names {path}: {}",
                    a.text
                );
                assert!(
                    !names_whole_any_case(&b.text, path),
                    "call {at} names {path}: {}",
                    b.text
                );
            }
        }
        // The replies were real answers, not two matching refusals.
        let statuses: Vec<_> = there.iter().map(|reply| reply.status).collect();
        use crate::agent::Status::{Executed, Failed};
        assert_eq!(
            statuses,
            [
                Executed, Executed, Executed, Failed, Executed, Failed, Executed, Failed, Executed
            ],
            "{there:#?}"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_the_agents_statement_selects_is_returned_as_is(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let own_path = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db")
            .display()
            .to_string();
        let (workspace, cx) = agent_sqlite(
            cx,
            &format!("CREATE TABLE t (note TEXT); INSERT INTO t VALUES ('{own_path}');"),
            "",
        );
        cx.executor().allow_parking();
        let answers = inject_sql(&workspace, cx, "Orders", "SELECT note FROM t");
        let reply = answer_of(cx, &answers);
        assert_eq!(reply.status, Status::Executed, "{}", reply.text);
        // The database's own data, so DBDelve leaves it exactly as stored.
        let row = serde_json::to_string(&[&own_path]).expect("a row serializes");
        assert!(
            reply.text.lines().any(|line| line == row),
            "{row} is not a line of {}",
            reply.text
        );
        quit(workspace, cx);
    });
}

/// D-26: a reply builder is handed what the database holds, never what a
/// profile says about where it is or how to get in.
#[test]
fn no_reply_builder_takes_a_connection_config_or_a_profile() {
    let source = include_str!("agent_read.rs");
    for builder in [
        "schemas_text",
        "tables_text",
        "structure_text",
        "plan_reply",
    ] {
        let start = source
            .find(&format!("fn {builder}("))
            .unwrap_or_else(|| panic!("{builder} is gone from agent_read.rs"));
        let signature = &source[start..start + source[start..].find('{').expect("a body")];
        for held in ["ConnectionConfig", "Profile"] {
            assert!(!signature.contains(held), "{builder} takes {held}");
        }
    }
}

fn disconnect(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(Disconnect), cx));
    settle(cx);
}

/// What the palette up lists, whatever scope each row is in.
fn palette_labels(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        let palette = workspace.palette.as_ref().expect("the palette is up");
        palette
            .read(cx)
            .labels()
            .into_iter()
            .map(str::to_string)
            .collect()
    })
}

fn buffers_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.profiles[0]
            .session
            .queries
            .iter()
            .map(|tab| tab.editor.read(cx).value().to_string())
            .collect()
    })
}

#[gpui::test]
fn disconnect_leads_home_and_a_relaunch_stays_there(cx: &mut TestAppContext) {
    // CI's live job sets `PG*`, whose profile would be put in front on launch.
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        let offered = palette_labels(&workspace, cx);
        assert!(offered.iter().any(|label| label == "Disconnect"));
        assert!(offered.iter().any(|label| label == "Disconnect from E2E"));
        cx.simulate_keystrokes("escape");
        settle(cx);

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            assert!(matches!(workspace.profiles[0].state, ProfileState::Idle));
        });
        assert_eq!(buffers_of(&workspace, cx), ["select 1"]);
        assert_eq!(store::peek_profiles(&[]).unwrap().active, None);
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "the welcome surface takes the keyboard"
            )
        });

        // Home lists the connections and nothing that needs one in front.
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::actions::SwitchConnection), cx)
        });
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::Search(Scope::Connections)));
        let offered = palette_labels(&workspace, cx);
        assert!(offered.iter().any(|label| label == "E2E"));
        assert!(!offered.iter().any(|label| label.starts_with("Disconnect")));
        cx.simulate_keystrokes("escape");
        settle(cx);

        // A second launch over the same home, as `launch` builds it.
        drop(workspace);
        let mut relaunched = None;
        let (_, cx) = cx.cx.add_window_view(|window, cx| {
            let entity = cx.new(|cx| Workspace::new(Vec::new(), window, cx));
            relaunched = Some(entity.clone());
            Root::new(entity, window, cx)
        });
        let workspace = relaunched.expect("the window builds the workspace");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            assert!(matches!(workspace.profiles[0].state, ProfileState::Idle));
        });
        assert_eq!(buffers_of(&workspace, cx), ["select 1"]);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_connection_picked_on_the_welcome_surface_comes_in_front_connected(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        disconnect(cx);
        let row = cx
            .debug_bounds("welcome-connection-0")
            .expect("the connection is listed");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the picked one is in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
        });
        assert_eq!(
            store::peek_profiles(&[]).unwrap().active.as_deref(),
            Some("e2e")
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_is_refused_while_a_grid_holds_unapplied_edits(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text); \
             insert into birds values (1, 'wren');",
        );
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().set_pending(
                0,
                1,
                crate::result_grid::NewValue::Value("robin".into())
            ));
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection stays in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            let notice = profile.session.notice.as_deref().unwrap_or_default();
            assert!(
                notice.ends_with("so the connection was not closed."),
                "{notice}"
            );
        });

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_is_refused_while_an_export_runs(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().expect("a connection in front");
            profile.session.exporting = Some(crate::session::Exporting {
                file: "birds.csv".into(),
                started: std::time::Instant::now(),
                progress: Default::default(),
            });
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection stays in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            let notice = profile.session.notice.as_deref().unwrap_or_default();
            assert_eq!(
                notice,
                "An export to birds.csv is running, so the connection was not closed."
            );
        });

        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().expect("in front").session.exporting = None;
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_stops_what_a_relation_tab_holds_the_connection_for(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
        cx.simulate_click(row, gpui::Modifiers::default());
        settle(cx);
        // A count still out, which a run left going is the same as: a task
        // holding a clone of the connection until the server answers.
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().expect("a profile is open");
            let tab = profile
                .session
                .objects
                .first_mut()
                .expect("the table opened");
            let ObjectBody::Relation { count, .. } = &mut tab.body else {
                panic!("a relation tab");
            };
            *count = crate::session::RowCount::Counting {
                filter: String::new(),
                started: std::time::Instant::now(),
                cancel: CancelToken::default(),
                cancelling: false,
            };
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            let tab = &workspace.profiles[0].session.objects[0];
            assert!(matches!(
                &tab.body,
                ObjectBody::Relation {
                    count: crate::session::RowCount::Counting {
                        cancelling: true,
                        ..
                    },
                    ..
                }
            ));
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_closes_an_import_dialog_that_is_not_running(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let file = store::dbdelve_directory().unwrap().join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        let profile = workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile().unwrap().id.clone();
            workspace.open_import_file(
                profile.clone(),
                None,
                "main".into(),
                "birds".into(),
                file,
                window,
                cx,
            );
            profile
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let found = workspace.profiles.iter().find(|found| found.id == profile);
            assert!(found.unwrap().session.import_rows.is_none());
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn files_opened_while_connecting_are_not_run_by_a_later_connect(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        settle(cx);
        cx.executor().allow_parking();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        // In one update, so the connect the file starts cannot land between.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_data_files(vec![file], window, cx);
            assert!(!workspace.runs_on_connect.is_empty());
            workspace.disconnect(&Disconnect, window, cx);
            assert!(workspace.runs_on_connect.is_empty());
        });
        settle(cx);

        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the files profile is in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
        });
        query_tab(&workspace, cx, |tab, _, cx| {
            assert!(matches!(tab.query, QueryState::Idle));
            assert!(
                !tab.editor.read(cx).value().is_empty(),
                "the statement stays"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn home_draws_an_agents_approval_and_the_visibility_dialog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        disconnect(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.profile().is_none()));

        run_command(&workspace, cx, Command::ShowToAgents(0));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-visibility"));
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);

        let (_, answers) = inject_approval(&workspace, cx, "Orders", UPDATE_ONE);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));
        approve_front(cx);
        assert_eq!(
            answer_of(cx, &answers).status,
            crate::agent::Status::Executed
        );
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
    });
}

/// How long the stand-in write runner holds an approved write before it opens
/// anything: the test moves the clock to let it start and land.
const WRITE_HOLD: std::time::Duration = std::time::Duration::from_secs(10);

/// A write that stays running for `hold` of the test scheduler's time and then
/// runs for real, so a test decides when it starts and lands.
fn held_write(
    hold: std::time::Duration,
    executor: &gpui::BackgroundExecutor,
    config: ConnectionConfig,
    profile_id: String,
    sql: String,
    engine: Engine,
) -> gpui::Task<super::agent_write::Wrote> {
    let executor = executor.clone();
    executor.clone().spawn(async move {
        executor.timer(hold).await;
        super::agent_write::open_for_write(&executor, config, profile_id, sql, engine).await
    })
}

/// Approved writes are held for [`WRITE_HOLD`] before they run.
fn hold_writes(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update(cx, |workspace, _| {
        workspace.agent.write_runner = Some(|executor, config, id, sql, engine| {
            held_write(WRITE_HOLD, executor, config, id, sql, engine)
        });
    });
}

fn writes_running(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<crate::agent::CallId> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .agent
            .writes
            .iter()
            .map(|write| write.id)
            .collect()
    })
}

/// The audit lines of one call, in the order they were written.
#[cfg(unix)]
fn audit_of(id: crate::agent::CallId) -> Vec<serde_json::Value> {
    audit_lines()
        .into_iter()
        .filter(|line| line["id"] == id.0)
        .collect()
}

fn executed() -> crate::agent::Reply {
    crate::agent::Reply {
        status: crate::agent::Status::Executed,
        text: "The server reported 1 row affected.".to_string(),
    }
}

/// A held write approved and running, with its call's reply channel.
fn running_write(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (
    crate::agent::CallId,
    std::sync::mpsc::Receiver<crate::agent::Reply>,
) {
    hold_writes(workspace, cx);
    let (id, answers) = inject_update(workspace, cx, Some("Orders"));
    approve_front(cx);
    assert_eq!(writes_running(workspace, cx), [id], "the write is running");
    assert_eq!(email_of(1).as_deref(), Some("a@x"), "and has not landed");
    (id, answers)
}

/// Let the held write start, run and land.
fn land_write(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(WRITE_HOLD);
    settle(cx);
}

#[cfg(unix)]
#[gpui::test]
fn a_hangup_while_a_write_runs_lets_it_finish_and_audits_no_requester(cx: &mut TestAppContext) {
    use crate::agent::{Inbound, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, answers) = running_write(&workspace, cx);

        workspace.update(cx, |workspace, cx| {
            workspace.on_agent_message(Inbound::Hangup { id }, cx)
        });
        assert_eq!(
            writes_running(&workspace, cx),
            [id],
            "a hangup stops nothing"
        );
        assert!(answers.try_recv().is_err(), "nothing was said yet");

        land_write(cx);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        assert_eq!(answers.try_recv().ok(), Some(executed()));
        let entry = agent_log(&workspace, cx)
            .into_iter()
            .find(|entry| entry.id == id)
            .expect("the call is logged");
        assert_eq!(entry.status, Status::Executed);
        assert!(entry.requester_gone);
        let lines = audit_of(id);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["phase"], "started");
        assert_eq!(lines[1]["phase"], "outcome");
        assert_eq!(lines[1]["requester_gone"], true);
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_hangup_the_window_has_not_heard_still_marks_a_running_write_as_gone(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        hold_writes(&workspace, cx);
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        // The socket's own flag, as its watcher sets it, and no `Hangup` for
        // the window to hear.
        let gone = workspace.read_with(cx, |workspace, _| {
            workspace.agent.calls[0].approval.gone.clone()
        });
        approve_front(cx);
        assert_eq!(writes_running(&workspace, cx), [id]);
        gone.store(true, std::sync::atomic::Ordering::Release);

        land_write(cx);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        assert_eq!(answers.try_recv().ok(), Some(executed()));
        let entry = agent_log(&workspace, cx)
            .into_iter()
            .find(|entry| entry.id == id)
            .expect("the call is logged");
        assert_eq!(entry.status, Status::Executed);
        assert!(
            entry.requester_gone,
            "known from the flag, not from Undelivered"
        );
        assert_eq!(audit_of(id)[1]["requester_gone"], true);
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn the_kill_switch_rejects_waiting_approvals_and_lets_a_running_write_finish(
    cx: &mut TestAppContext,
) {
    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (running, running_answers) = running_write(&workspace, cx);
        let (waiting, waiting_answers) = inject_update(&workspace, cx, Some("Orders"));
        assert_eq!(pending_calls(&workspace, cx), [waiting]);

        run_command(&workspace, cx, Command::DisconnectAgents);
        assert_eq!(
            waiting_answers.try_recv().ok(),
            Some(Reply {
                status: Status::Rejected,
                text: "Agent access was turned off in DBDelve.".to_string(),
            })
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        assert_eq!(writes_running(&workspace, cx), [running]);
        assert!(
            running_answers.try_recv().is_err(),
            "the write was not answered"
        );

        land_write(cx);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        assert_eq!(running_answers.try_recv().ok(), Some(executed()));
        assert_eq!(
            audit_of(running).last().expect("an outcome")["phase"],
            "outcome"
        );
        assert_eq!(
            audit_of(running).last().expect("an outcome")["status"],
            "executed"
        );
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn hiding_a_profile_rejects_its_waiting_approvals_and_lets_its_running_write_finish(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (running, running_answers) = running_write(&workspace, cx);
        let (waiting, waiting_answers) = inject_update(&workspace, cx, Some("Orders"));
        assert_eq!(pending_calls(&workspace, cx), [waiting]);

        workspace.update(cx, |workspace, cx| {
            workspace.set_agent_visible("orders", false, cx)
        });
        settle(cx);
        assert_eq!(waiting_answers.try_recv().ok(), Some(swept()));
        assert!(pending_calls(&workspace, cx).is_empty());
        assert_eq!(writes_running(&workspace, cx), [running]);
        assert!(
            running_answers.try_recv().is_err(),
            "the write was not answered"
        );

        land_write(cx);
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        assert_eq!(running_answers.try_recv().ok(), Some(executed()));
        assert_eq!(
            audit_of(running).last().expect("an outcome")["status"],
            "executed"
        );
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_deadline_never_ends_an_approved_write(cx: &mut TestAppContext) {
    use super::agent_read::CALL_DEADLINE;
    use std::time::Duration;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        workspace.update(cx, |workspace, _| {
            workspace.agent.write_runner = Some(|executor, config, id, sql, engine| {
                held_write(Duration::from_secs(90), executor, config, id, sql, engine)
            });
        });
        let (id, answers) = inject_update(&workspace, cx, Some("Orders"));
        approve_front(cx);
        assert_eq!(writes_running(&workspace, cx), [id]);

        cx.executor()
            .advance_clock(CALL_DEADLINE + Duration::from_secs(1));
        settle(cx);
        assert_eq!(
            writes_running(&workspace, cx),
            [id],
            "the read deadline passed"
        );
        assert!(answers.try_recv().is_err(), "nothing has been replied");
        assert_eq!(email_of(1).as_deref(), Some("a@x"));

        cx.executor()
            .advance_clock(Duration::from_secs(90) - CALL_DEADLINE);
        settle(cx);
        assert!(writes_running(&workspace, cx).is_empty());
        assert_eq!(answers.try_recv().ok(), Some(executed()));
        assert_eq!(email_of(1).as_deref(), Some("z@x"));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn quitting_with_a_running_write_asks_nothing_and_leaves_its_started_line(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, _answers) = running_write(&workspace, cx);
        assert_eq!(front_dialog(&workspace, cx), None, "no dialog is up");

        quit(workspace, cx);
        let lines = audit_of(id);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["phase"], "started");
        assert_eq!(email_of(1).as_deref(), Some("a@x"), "the write never ran");
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_waiting_approval_holds_no_connection_and_no_task_but_its_timers(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        let (id, _answers) = inject_update(&workspace, cx, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-approval"));

        // GPUI's test scheduler is asked directly: a parked run returns while
        // timers are pending too, so it cannot show that nothing is queued on
        // the background executor.
        let scheduler = cx.cx.dispatcher.scheduler().clone();
        let nothing_on_the_background_executor = || {
            assert_eq!(
                scheduler.pending_task_counts().1,
                0,
                "a background task is queued"
            );
            assert!(
                !scheduler.tick_background_only(),
                "the background executor has work"
            );
        };
        nothing_on_the_background_executor();

        cx.executor().advance_clock(super::agent::ARM_DELAY);
        settle(cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.agent.armed),
            Some(id),
            "the arm fired"
        );
        nothing_on_the_background_executor();

        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.agent.lanes.is_empty(), "a connection was opened");
            assert!(workspace.agent.writes.is_empty(), "a write was started");
        });
        assert_eq!(pending_calls(&workspace, cx), [id]);
        assert_eq!(email_of(1).as_deref(), Some("a@x"));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_running_write_keeps_the_agent_mark_lit(cx: &mut TestAppContext) {
    use crate::ui::AgentMark;

    store::tests::with_home(|| {
        let (workspace, cx) = agent_sqlite(cx, ORDERS_SCHEMA, "mode = \"read-write\"");
        // The socket's own threads touch the scheduler's wakers when the
        // bridge ends.
        cx.executor().allow_parking();
        let path = crate::agent::tests::short_socket_path();
        workspace
            .update(cx, |workspace, cx| {
                workspace.start_bridge_at(path.clone(), cx)
            })
            .expect("the socket binds");
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));

        running_write(&workspace, cx);
        assert_eq!(
            agent_mark_state(&workspace, cx),
            Some(AgentMark::Connected { waiting: 0 }),
            "no shim is attached and nothing waits, and a write runs"
        );

        land_write(cx);
        assert_eq!(agent_mark_state(&workspace, cx), Some(AgentMark::Listening));
        quit(workspace, cx);
        let _ = std::fs::remove_dir_all(path.parent().expect("the socket has a directory"));
    });
}

/// Whether the sidebar draws the connections tree: only it draws rows named
/// `connections-row-{n}`.
fn connections_tree_drawn(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("connections-row-0").is_some()
}

fn view_mode_in_effect(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> ViewMode {
    workspace.read_with(cx, |workspace, _| workspace.view_mode())
}

/// `b`, in the Zoo project, and `a`, in none, with `settings` as `[settings]`
/// keys; `b` in front and connected.
fn zoo_and_a<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        settings,
        &format!(
            "[[projects]]\nname = \"Zoo\"\nconnections = [\"b\"]\n{}{}",
            sqlite_profile("b", "Flocks", ""),
            sqlite_profile("a", "Birds", ""),
        ),
    );
    workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
    settle(cx);
    (workspace, cx)
}

fn activate_by_id(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str) {
    workspace.update(cx, |workspace, cx| {
        let index = workspace
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .expect("the profile was restored");
        workspace.activate(index, cx);
    });
    settle(cx);
}

#[gpui::test]
fn a_project_can_show_every_connection_while_the_global_setting_stays_simple(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert_eq!(front_profile(&workspace, cx), "b");
        assert!(
            !connections_tree_drawn(cx),
            "the profile's own tree is drawn"
        );

        run_command(
            &workspace,
            cx,
            Command::SetProjectViewMode(0, Some(ViewMode::Complex)),
        );
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert!(connections_tree_drawn(cx), "the connections tree is drawn");

        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(saved.projects[0].view_mode.as_deref(), Some("complex"));
        let global = saved.settings.and_then(|settings| settings.view_mode);
        assert!(
            matches!(global.as_deref(), None | Some("simple")),
            "{global:?}"
        );

        // The project in front decides: a connection in no project follows
        // the global setting, and Zoo's own again follows Zoo's.
        activate_by_id(&workspace, cx, "a");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert!(!connections_tree_drawn(cx), "the simple tree is back");
        activate_by_id(&workspace, cx, "b");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert!(connections_tree_drawn(cx));

        run_command(&workspace, cx, Command::SetProjectViewMode(0, None));
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        let file = std::fs::read_to_string(
            store::dbdelve_directory()
                .expect("the home has a data directory")
                .join("profiles.toml"),
        )
        .expect("the file reads");
        assert!(!file.contains("complex"), "{file}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_the_view_mode_moves_no_tab_and_no_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = workspace
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("a profile opens on a query tab")
                .editor
                .clone();
            editor.update(cx, |input, cx| input.set_value("select 1", window, cx));
        });
        settle(cx);
        let snapshot = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            let text = buffer(workspace, cx);
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("b is in front").session;
                (
                    session.queries.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                    text,
                    session.active,
                    workspace.active,
                    workspace.next_generation,
                    profile_state_of(workspace, "b"),
                    profile_state_of(workspace, "a"),
                )
            })
        };
        let before = snapshot(&workspace, cx);
        assert_eq!(before.6, "idle");

        for _ in 0..2 {
            run_command(
                &workspace,
                cx,
                Command::SetProjectViewMode(0, Some(ViewMode::Complex)),
            );
            assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
            run_command(&workspace, cx, Command::SetProjectViewMode(0, None));
            assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        }
        assert_eq!(snapshot(&workspace, cx), before);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_every_connection_switch_flips_the_sidebar_and_persists(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        // Tall enough to draw the General tab's rows, which are drawn only
        // while in view.
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(2000.)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_settings(window, cx)
        });
        settle(cx);
        let click = |selector: &'static str, cx: &mut VisualTestContext| {
            let bounds = cx.debug_bounds(selector).expect("the switch is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
        };
        let saved = || {
            store::peek_profiles(&[])
                .and_then(|list| list.settings)
                .expect("the settings are saved")
        };

        click("sidebar-every-connection", cx);
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert_eq!(saved().view_mode.as_deref(), Some("complex"));
        click("sidebar-every-connection", cx);
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert_eq!(saved().view_mode.as_deref(), Some("simple"));

        click("sidebar-current-project", cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.listing_scope()),
            ListingScope::Current
        );
        assert_eq!(saved().listing_scope.as_deref(), Some("current"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_project_can_list_only_its_own_connections_while_the_setting_lists_all(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "view_mode = \"complex\"");
        assert_eq!(
            connections_rows(&workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 0)
                .map(|(_, id)| id)
                .collect::<Vec<_>>(),
            [NO_PROJECT_ROW.to_string(), project_row("Zoo")],
            "every project is listed"
        );

        run_command(
            &workspace,
            cx,
            Command::SetProjectListing(0, Some(ListingScope::Current)),
        );
        assert_eq!(
            connections_rows(&workspace, cx),
            [(0, connection_row("b"))],
            "Zoo lists its own connections, with no group row"
        );
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(saved.projects[0].listing_scope.as_deref(), Some("current"));

        // A connection in no project follows the setting, which lists all.
        activate_by_id(&workspace, cx, "a");
        assert_eq!(
            connections_rows(&workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 0)
                .count(),
            2
        );
        quit(workspace, cx);
    });
}

fn connections_label(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<String> {
    let index = connections_row(workspace, cx, id)?;
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        Some(tree.entry(index)?.item().label.to_string())
    })
}

fn selected_connections_row(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        tree.selected_item().map(|item| item.id.to_string())
    })
}

#[gpui::test]
fn adding_renaming_and_removing_a_connection_redraws_the_connections_tree(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let config = workspace.read_with(cx, |workspace, _| workspace.profiles[0].config.clone());
        let id_named = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, name: &str| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .map(|profile| profile.id.clone())
            })
        };
        let listed = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            connections_rows(workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 1)
                .count()
        };
        assert_eq!(listed(&workspace, cx), 2);

        // The form's save of a new connection.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_profile(
                "Extra".into(),
                config.clone(),
                None,
                crate::sql::Mode::ReadOnly,
                crate::connection_form::Origin::Form,
                window,
                cx,
            );
        });
        settle(cx);
        let id = id_named(&workspace, cx, "Extra").expect("the connection was added");
        let row = connection_row(&id);
        assert_eq!(listed(&workspace, cx), 3);
        assert_eq!(
            connections_label(&workspace, cx, &row).as_deref(),
            Some("Extra")
        );

        // An edit, its name included, keeps the row.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.save_profile(&id, "Renamed".into(), config.clone(), None, window, cx);
        });
        settle(cx);
        assert_eq!(
            connections_label(&workspace, cx, &row).as_deref(),
            Some("Renamed")
        );
        assert_eq!(listed(&workspace, cx), 3);

        // A duplicate is a form until it is saved, and the save adds the row.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.duplicate_profile(0, window, cx);
        });
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.submit_connection(&SubmitConnection, window, cx);
        });
        settle(cx);
        assert_eq!(listed(&workspace, cx), 4);

        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == id)
                .expect("the connection is still there");
            workspace.remove_profile(index, cx);
        });
        settle(cx);
        assert_eq!(connections_row(&workspace, cx, &row), None);
        assert_eq!(listed(&workspace, cx), 3);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnecting_redraws_the_connection_in_front_as_not_open(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let a = connection_row("a");
        let schema = scoped_row("a", None, "schema-main");
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "connected");
        let (_, rows) = connections_item(&workspace, cx, &a).expect("a is listed");
        assert!(rows.iter().any(|(id, ..)| id == &schema), "{rows:?}");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.disconnect(&Disconnect, window, cx);
        });
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "idle");
        assert_eq!(
            connections_item(&workspace, cx, &a),
            Some((
                false,
                vec![(scoped_row("a", None, "loading"), LOADING.to_string(), true)]
            ))
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn moving_a_connection_to_another_project_keeps_its_rows_and_folds(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.create_project("Other".into(), cx)
        });
        // `b` in front and connected, its schema open and a relation selected.
        activate_by_id(&workspace, cx, "b");
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        let b = connection_row("b");
        let schema = scoped_row("b", None, "schema-main");
        let birds = scoped_row("b", None, &explorer::relation_row("main", "birds"));
        for row in [&b, &schema] {
            let at = connections_row(&workspace, cx, row).expect("the row is listed");
            click_connections_row(cx, at);
        }
        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |tree, cx| {
                // A relation is under its category, which a reveal opens.
                let id = birds.clone().into();
                tree.reveal_item(&id, gpui::ScrollStrategy::Top, cx);
                let row = tree.index_of(&id);
                assert!(row.is_some(), "the schema's relations are listed");
                tree.set_selected_index(row, cx);
            });
        });
        settle(cx);

        // What the rows of `b` are, and the group row they are drawn under.
        let kept = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            let group = connections_rows(workspace, cx)
                .into_iter()
                .take_while(|(_, id)| id != &b)
                .filter(|(depth, _)| *depth == 0)
                .last()
                .map(|(_, id)| id);
            (
                connections_row(workspace, cx, &b).is_some(),
                connections_item(workspace, cx, &schema).map(|(open, _)| open),
                selected_connections_row(workspace, cx),
                group,
            )
        };
        let rows = (true, Some(true), Some(birds.clone()));
        let before = kept(&workspace, cx);
        assert_eq!(
            before,
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Zoo")))
        );

        let index = workspace.read_with(cx, |workspace, _| {
            workspace.profiles.iter().position(|p| p.id == "b").unwrap()
        });
        workspace.update(cx, |workspace, cx| {
            workspace.move_to_project(index, Some("Other"), cx)
        });
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Other"))),
            "moved"
        );

        workspace.update(cx, |workspace, cx| {
            workspace.rename_project("Other", "Renamed".into(), cx)
        });
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Renamed"))),
            "its project renamed"
        );

        workspace.update(cx, |workspace, cx| workspace.remove_project("Renamed", cx));
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2, Some(NO_PROJECT_ROW.to_string())),
            "its project deleted"
        );
        quit(workspace, cx);
    });
}

/// Where the connections tree drew `kind` (`logo`, `name`, `agent`, `host`,
/// `count`, `status`, `fill`) for the connection `id`, if it did.
fn connection_part(
    cx: &mut VisualTestContext,
    kind: &str,
    id: &str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-{kind}-{id}").leak();
    cx.debug_bounds(selector)
}

/// The status the connections tree's dot for `id` was drawn in.
fn connection_dot(cx: &mut VisualTestContext, id: &str) -> Option<&'static str> {
    ["idle", "connecting", "connected", "failed"]
        .into_iter()
        .find(|status| {
            let selector: &'static str = format!("connections-dot-{id}-{status}").leak();
            cx.debug_bounds(selector).is_some()
        })
}

/// Two SQLite connections in complex mode, `a` in front and connected, each
/// with its own extra keys, and `settings` among the `[settings]`.
fn coloured_connections<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    a: &str,
    b: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        &format!("view_mode = \"complex\"\n{settings}"),
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", a),
            sqlite_profile("b", "Flocks", b)
        ),
    );
    settle(cx);
    (workspace, cx)
}

/// One DuckDB profile as `profiles.toml` spells it, over the file at `path`,
/// which is created holding `t`. A DuckDB connection counts the schemas it
/// lists; a SQLite one is flat and counts nothing.
fn duckdb_profile(id: &str, name: &str, path: &std::path::Path, extra: &str) -> String {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the directory must be creatable");
    }
    ::duckdb::Connection::open(path)
        .and_then(|connection| connection.execute_batch("CREATE TABLE t (id INTEGER);"))
        .expect("the schema must apply");
    format!(
        "[[profiles]]\nid = {id:?}\nname = {name:?}\nhost = \"\"\ndatabase = \"\"\n\
         user = \"\"\nengine = \"duckdb\"\npath = {:?}\n{extra}\n",
        path.display().to_string()
    )
}

#[gpui::test]
fn a_connection_row_carries_its_logo_status_marker_and_colour(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // Left alone, CI's live job puts a connection from `PG*` among these.
        crate::IGNORE_ENVIRONMENT.set(true);
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                duckdb_profile(
                    "a",
                    "Birds",
                    &directory.join("birds.duckdb"),
                    &format!("color = \"red\"\n{VISIBLE_RW}")
                ),
                sqlite_profile("b", "Flocks", "agent_visible = false")
            ),
        );
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "connected");

        let [logo, name, agent, host, count, status] =
            ["logo", "name", "agent", "host", "count", "status"].map(|kind| {
                connection_part(cx, kind, "a")
                    .unwrap_or_else(|| panic!("the row of `a` draws its {kind}"))
            });
        assert!(
            connection_part(cx, "fill", "a").is_some(),
            "red fills its row"
        );
        let lefts = [logo, name, agent, host, count, status].map(|bounds| bounds.left());
        assert!(
            lefts.windows(2).all(|pair| pair[0] < pair[1]),
            "logo, name, marker, host, count, status, left to right: {lefts:?}"
        );
        assert_eq!(connection_dot(cx, "a"), Some("connected"));

        for kind in ["logo", "host", "status"] {
            assert!(
                connection_part(cx, kind, "b").is_some(),
                "`b` draws its {kind}"
            );
        }
        for kind in ["agent", "fill"] {
            assert!(
                connection_part(cx, kind, "b").is_none(),
                "`b` is hidden from agents and uncoloured, and draws no {kind}"
            );
        }
        assert_eq!(connection_dot(cx, "b"), Some("idle"), "never absent");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_long_host_gives_way_before_the_connection_name(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        let long = directory
            .join("a".repeat(48))
            .join("b".repeat(48))
            .join("c".repeat(48))
            .join("birds.duckdb");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "view_mode = \"complex\"",
            &duckdb_profile("a", "Birds", &long, "agent_visible = true"),
        );
        settle(cx);
        let name_before = connection_part(cx, "name", "a").expect("the name is drawn");
        let host_before = connection_part(cx, "host", "a").expect("the host is drawn");

        workspace.update_in(cx, |workspace, window, cx| {
            let narrowest = gpui::px(crate::theme::layout::SIDEBAR_MIN_WIDTH);
            workspace.sidebar_width.set(narrowest);
            workspace
                .shell_split
                .update(cx, |state, cx| state.resize_panel(0, narrowest, window, cx));
        });
        settle(cx);

        let name = connection_part(cx, "name", "a").expect("the name is drawn");
        let host = connection_part(cx, "host", "a").expect("the host is drawn");
        let count = connection_part(cx, "count", "a").expect("the count is drawn");
        assert!(
            host.size.width < host_before.size.width,
            "the narrow sidebar took its room from the host"
        );
        assert_eq!(
            name.size.width, name_before.size.width,
            "the name keeps its text"
        );
        assert!(
            name.right() <= host.left(),
            "the host starts after the name"
        );
        assert!(
            host.right() <= count.left(),
            "the host ends before the count"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_status_dot_follows_the_users_connection_and_never_the_agents(cx: &mut TestAppContext) {
    use crate::agent::Status;

    store::tests::with_home(|| {
        // `b` in front and connected; `a` is agent-visible and never
        // connected by the user.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table t (id integer);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("b", "Flocks", ""),
                sqlite_profile("a", "Orders", VISIBLE_RW)
            ),
        );
        settle(cx);
        assert_eq!(connection_dot(cx, "a"), Some("idle"), "idle is a status");

        cx.executor().allow_parking();
        let answers = inject_sql(&workspace, cx, "Orders", "SELECT id FROM t");
        assert_eq!(answer_of(cx, &answers).status, Status::Executed);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.agent.lanes.contains_key("a"),
                "the agent's lane is open"
            );
        });
        assert_eq!(profile_state(&workspace, cx, "a"), "idle");
        assert_eq!(
            connection_dot(cx, "a"),
            Some("idle"),
            "the agent's lane left the user's dot alone"
        );

        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                cx.emit(TreeEvent::Expanded(connection_row("a").into()));
            });
        });
        assert_eq!(profile_state(&workspace, cx, "a"), "connecting");
        cx.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(connection_dot(cx, "a"), Some("connecting"));
        settle(cx);
        assert_eq!(connection_dot(cx, "a"), Some("connected"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn two_connections_of_one_colour_are_two_tinted_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let tinted = format!("color = \"blue\"\n{VISIBLE_RW}");
        let (workspace, cx) = coloured_connections(cx, "", &tinted, &tinted);
        for id in ["a", "b"] {
            for kind in ["fill", "agent", "status"] {
                assert!(
                    connection_part(cx, kind, id).is_some(),
                    "`{id}` draws its own {kind}"
                );
            }
        }
        let (a, b) = (
            connection_part(cx, "fill", "a").unwrap(),
            connection_part(cx, "fill", "b").unwrap(),
        );
        assert_ne!(a.origin.y, b.origin.y, "two rows, not one");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_density_setting_sets_the_connections_trees_row_height(cx: &mut TestAppContext) {
    use crate::{sidebar::Density, theme::layout};

    store::tests::with_home(|| {
        let (workspace, cx) = coloured_connections(cx, "sidebar_density = \"compact\"", "", "");
        let height = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .size
                .height
        };
        assert_eq!(
            height(cx, "connections-row-0"),
            gpui::px(layout::chrome(22.))
        );

        workspace.update(cx, |workspace, cx| {
            workspace.set_density(Density::Comfortable, cx)
        });
        settle(cx);
        assert_eq!(
            height(cx, "connections-row-0"),
            gpui::px(layout::chrome(30.))
        );

        // The simple tree keeps its rows whatever the density.
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        let simple = height(cx, "explorer-row-0");
        for density in Density::ALL {
            workspace.update(cx, |workspace, cx| workspace.set_density(density, cx));
            settle(cx);
            assert_eq!(height(cx, "explorer-row-0"), simple, "{density:?}");
        }
        let saved = store::peek_profiles(&[]).expect("the file reads");
        let saved = saved.settings.and_then(|settings| settings.sidebar_density);
        assert_eq!(saved.as_deref(), Some(Density::ALL[2].slug()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn hiding_the_host_drops_it_from_connection_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = coloured_connections(cx, "", "", "");
        assert!(connection_part(cx, "host", "a").is_some());
        workspace.update(cx, |workspace, cx| workspace.set_show_host(false, cx));
        settle(cx);
        assert!(connection_part(cx, "host", "a").is_none());
        assert!(connection_part(cx, "host", "b").is_none());
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(
            saved
                .settings
                .and_then(|settings| settings.sidebar_show_host),
            Some(false)
        );
        workspace.update(cx, |workspace, cx| workspace.set_show_host(true, cx));
        settle(cx);
        assert!(connection_part(cx, "host", "a").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn with_sizes_off_no_size_read_starts(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_duckdb_with(cx, "sidebar_show_sizes = false", "mode = \"full\"");
        let reads = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.size_reads)
        };
        let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("it shows");
        click_connections_row(cx, at);
        attach_other(&workspace, cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("the attached database shows");
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(
                matches!(
                    profile.database_catalogs.get("other"),
                    Some(CatalogState::Loaded(..))
                ),
                "the other database loaded"
            );
        });
        assert_eq!(reads(&workspace, cx), 0, "no size was asked for");
        assert!(connection_part(cx, "count", "e2e").is_some(), "counts stay");

        workspace.update(cx, |workspace, cx| workspace.set_show_sizes(true, cx));
        settle(cx);
        assert_eq!(
            reads(&workspace, cx),
            2,
            "one for the connection and one for the loaded database"
        );
        quit(workspace, cx);
    });
}

/// Opens a tab with New Query Here on `database` and `schema` of the profile
/// in front, and waits for it to be the one in front.
fn new_query_here(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    database: Option<&str>,
    schema: Option<&str>,
) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.new_query_here(
                "e2e",
                database.map(str::to_string),
                schema.map(str::to_string),
                window,
                cx,
            );
        });
    });
    settle(cx);
}

/// The binding of the query tab in front.
fn front_binding(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::Binding> {
    query_tab(workspace, cx, |tab, _, _| tab.binding.clone())
}

/// The first cell of the result in the query tab in front.
fn answer(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    query_tab(workspace, cx, |tab, _, cx| {
        let grid = tab.results.read(cx);
        let result = grid.delegate().result();
        result.rows.first()?.get(0).map(str::to_string)
    })
}

/// Whether the profile in front has left a transaction open or changed its
/// session, as its own tabs' statements are read to.
fn profile_flags(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (bool, bool) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a profile is in front");
        (profile.in_transaction, profile.session_changed)
    })
}

#[gpui::test]
fn a_query_tab_opened_on_an_attached_database_runs_there_and_moves_nothing_else(
    cx: &mut TestAppContext,
) {
    use crate::session::Binding;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        let own = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });

        new_query_here(&workspace, cx, Some("other"), None);
        assert_eq!(
            front_binding(&workspace, cx),
            Some(Binding {
                database: "other".into(),
                schema: None
            })
        );
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        assert_eq!(
            buffer(&workspace, cx),
            "SELECT current_database()",
            "the text is what was typed"
        );
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert_eq!(profile.session.history[0].sql, "SELECT current_database()");
            assert_eq!(profile.active_database().as_deref(), Some("e2e"));
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
        });

        // A transaction of the user's on the shared session is that
        // session's, never the profile's.
        for statement in ["BEGIN", "SELECT current_database()", "ROLLBACK"] {
            run_in_front(&workspace, cx, statement);
            assert_eq!(profile_flags(&workspace, cx), (false, false), "{statement}");
        }
        assert_eq!(buffer(&workspace, cx), "ROLLBACK");

        // The profile's own tab is where it was.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.activate_tab(own, window, cx));
        });
        settle(cx);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        assert_eq!(own_database(&workspace, cx), "e2e");
        assert_eq!(profile_flags(&workspace, cx), (false, false));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn new_query_here_on_a_connection_row_opens_an_ordinary_tab(cx: &mut TestAppContext) {
    use crate::session::Binding;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, None, None);
        assert_eq!(
            front_binding(&workspace, cx),
            None,
            "a connection row names no database"
        );
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.profile().unwrap().bound.is_empty(),
                "the profile's own session ran it"
            );
        });

        new_query_here(&workspace, cx, Some(""), None);
        assert_eq!(
            front_binding(&workspace, cx),
            None,
            "a blank name is the login default"
        );

        new_query_here(&workspace, cx, Some("other"), None);
        assert_eq!(
            front_binding(&workspace, cx),
            Some(Binding {
                database: "other".into(),
                schema: None
            })
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn new_query_in_complex_mode_opens_an_unbound_tab_on_the_profiles_own_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_query(&crate::actions::NewQuery, window, cx);
            });
        });
        settle(cx);
        assert_eq!(front_binding(&workspace, cx), None);
        assert_eq!(profile_flags(&workspace, cx), (false, false));
        run_in_front(&workspace, cx, "BEGIN");
        assert_eq!(
            profile_flags(&workspace, cx),
            (true, false),
            "the profile's own session, which keeps its transaction controls"
        );
        run_in_front(&workspace, cx, "ROLLBACK");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_session_a_bound_run_opened_is_kept_and_its_database_reads_over_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().bound.is_empty());
        });
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT current_database()");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            // Left at "Loading" the row would never read the database: a
            // registered session counts as already being read.
            assert!(
                matches!(
                    profile.database_catalogs.get("other"),
                    Some(CatalogState::Loaded(..))
                ),
                "the database's catalog was read over the session the run kept"
            );
        });

        // A second tab bound to the same database shares the session.
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().bound.len(), 1);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_edit_applied_in_a_bound_tab_writes_to_its_database(cx: &mut TestAppContext) {
    use crate::session::Refresh;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        // `u` is in `other` and in no other database: run on the profile's
        // own connection, this would fail.
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        workspace.update(cx, |workspace, cx| {
            workspace.execute_unchecked(
                "INSERT INTO u VALUES (7)".into(),
                tab,
                Some(Refresh::Statement("SELECT count(*) FROM u".into())),
                false,
                None,
                None,
                cx,
            );
        });
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { .. } => {}
            QueryState::Failed(error) => panic!("the edit failed: {error}"),
            _ => panic!("the edit never completed"),
        });
        assert_eq!(answer(&workspace, cx).as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn every_row_of_a_bound_tabs_result_is_not_fetched_on_the_profiles_own_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        run_in_front(&workspace, cx, "SELECT 1");
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.refetch().is_ok()),
            "an ordinary tab's statement is fetched again as it always was"
        );
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT 1");
        let refusal = workspace.read_with(cx, |workspace, _| workspace.refetch());
        assert_eq!(
            refusal.err(),
            Some(
                "this tab runs on the session of the database it is bound to, \
                 and every row would be fetched on the connection's own."
            )
        );
        quit(workspace, cx);
    });
}

/// A second window over the same home, as `launch` builds it, once the first
/// has gone: what a relaunch reads is what the first left in `profiles.toml`.
fn relaunch(
    workspace: Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    cx.update(|window, _| window.remove_window());
    drop(workspace);
    cx.cx.run_until_parked();
    let mut relaunched = None;
    let (_, cx) = cx.cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(Vec::new(), window, cx));
        relaunched = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    settle(cx);
    (relaunched.expect("the window builds the workspace"), cx)
}

/// Brings the query tab `bound` says is bound, or is not, to the front.
fn front_the_tab(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, bound: bool) {
    let tab = workspace.read_with(cx, |workspace, _| {
        let id = workspace
            .profile()
            .and_then(|profile| {
                profile
                    .session
                    .queries
                    .iter()
                    .find(|tab| tab.binding.is_some() == bound)
            })
            .expect("the tab is open")
            .id;
        Tab::Query(id)
    });
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.activate_tab(tab, window, cx));
    });
    settle(cx);
}

#[gpui::test]
fn a_bound_tab_keeps_its_binding_across_a_relaunch(cx: &mut TestAppContext) {
    use crate::session::Binding;
    let other = Binding {
        database: "other".into(),
        schema: None,
    };
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        let saved = store::peek_profiles(&[]).expect("the file reads");
        let tabs = &saved.profiles[0].open_queries;
        assert_eq!(
            tabs.iter()
                .map(|tab| tab.database.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("other")],
            "the binding is written with its tab"
        );

        let (workspace, cx) = relaunch(workspace, cx);
        assert_eq!(front_binding(&workspace, cx), Some(other.clone()));

        // `other` was attached to the first window's instance and is not on
        // the new one: the run says what the open said, and the tab is
        // still bound to `other`.
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("the bound tab is in front")
                .editor
                .clone();
            editor.update(cx, |input, cx| {
                input.set_value("SELECT current_database()", window, cx)
            });
        });
        cx.dispatch_action(crate::actions::RunQuery);
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Failed(error) => assert!(
                error.message.contains("other"),
                "the open's own words name the database: {}",
                error.message
            ),
            _ => panic!("a run on a database the server lacks must fail"),
        });
        assert_eq!(front_binding(&workspace, cx), Some(other.clone()));

        front_the_tab(&workspace, cx, false);
        attach_other(&workspace, cx);
        front_the_tab(&workspace, cx, true);
        assert_eq!(front_binding(&workspace, cx), Some(other));
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        quit(workspace, cx);
    });
}

/// The transaction state of the profile in front: its auto-commit switch, an
/// open transaction, and how many statements its history holds.
fn transaction_state(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (bool, bool, usize) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a profile is in front");
        (
            profile.auto_commit,
            profile.in_transaction,
            profile.session.history.len(),
        )
    })
}

#[gpui::test]
fn transaction_controls_are_refused_on_a_bound_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT current_database()");
        let before = transaction_state(&workspace, cx);
        assert_eq!((before.0, before.1), (true, false));

        let controls: [(&str, Box<dyn gpui::Action>); 3] = [
            (
                "Toggle Auto-commit",
                Box::new(crate::actions::ToggleAutoCommit),
            ),
            ("Commit", Box::new(crate::actions::CommitTransaction)),
            ("Roll Back", Box::new(crate::actions::RollbackTransaction)),
        ];
        for (name, action) in controls {
            workspace.update(cx, |workspace, _| workspace.clear_notice());
            cx.update(|window, cx| window.dispatch_action(action, cx));
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.profile().unwrap().session.notice.as_deref(),
                    Some(crate::session::BOUND_TRANSACTION),
                    "{name}"
                );
            });
            assert_eq!(
                buffer(&workspace, cx),
                "SELECT current_database()",
                "{name}"
            );
            assert_eq!(transaction_state(&workspace, cx), before, "{name}");
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_unbound_tab_in_complex_mode_keeps_its_transaction_controls(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_query(&crate::actions::NewQuery, window, cx)
            });
        });
        settle(cx);
        assert_eq!(front_binding(&workspace, cx), None);

        cx.dispatch_action(crate::actions::ToggleAutoCommit);
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "BEGIN");
        let (auto_commit, in_transaction, _) = transaction_state(&workspace, cx);
        assert!(!auto_commit, "the switch turned off");
        assert!(in_transaction, "the BEGIN ran on the profile's own session");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_use_in_a_bound_tab_is_undone_before_its_next_run(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "USE e2e");
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(
                session
                    .tab_notice
                    .as_ref()
                    .map(|(_, notice)| notice.as_str()),
                Some("The next run in this tab starts again on other.")
            );
        });
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        assert!(
            !profile_flags(&workspace, cx).1,
            "the profile's own session is unmarked"
        );
        quit(workspace, cx);
    });
}

/// The DuckDB profile in complex mode with `other` attached, each file
/// holding a `t (name TEXT, id INTEGER PRIMARY KEY)` of its own: `own` in the
/// connection's, `other` in the attached one.
fn two_databases_each_holding_t(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_duckdb_seeded(
        cx,
        "",
        "mode = \"full\"",
        "CREATE TABLE t (name TEXT, id INTEGER PRIMARY KEY); INSERT INTO t VALUES ('own', 1);",
        "CREATE TABLE t (name TEXT, id INTEGER PRIMARY KEY); INSERT INTO t VALUES ('other', 1);",
    );
    let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("the connection shows");
    click_connections_row(cx, at);
    attach_other(&workspace, cx);
    (workspace, cx)
}

/// Clicks down the tree to `t` under `database` of the connection `e2e`,
/// opening each folder on the way, and the table last.
fn click_t_under(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, database: &str) {
    let rows = [
        database_row("e2e", database),
        scoped_row("e2e", Some(database), "schema-main"),
        scoped_row("e2e", Some(database), "category-Tables-main"),
        scoped_row("e2e", Some(database), &explorer::relation_row("main", "t")),
    ];
    for id in rows {
        // A folder already open stays open: only a row that is not showing
        // is one its parent has yet to reveal.
        let at =
            connections_row(workspace, cx, &id).unwrap_or_else(|| panic!("{id} is not showing"));
        click_connections_row(cx, at);
    }
}

/// The database and the first cell of the object tab in front.
fn front_object_database_and_cell(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<(Option<String>, Option<String>)> {
    workspace.read_with(cx, |workspace, cx| {
        let tab = workspace.profile()?.session.active_object()?;
        let ObjectBody::Relation { results, .. } = &tab.body else {
            return None;
        };
        let cell = results.read(cx).delegate().cell(0, 0).map(str::to_string);
        Some((tab.database.clone(), cell))
    })
}

#[gpui::test]
fn a_table_under_another_database_opens_that_databases_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into()))),
            "the table under `other` reads `other`'s row"
        );
        let first = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });

        click_t_under(&workspace, cx, "e2e");
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((None, Some("own".into()))),
            "the same `main.t` under the connection's own database reads its own row"
        );
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(session.objects.len(), 2, "two tabs, both listed");
            assert_ne!(session.active, first);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_tab_of_another_database_counts_and_describes_it_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        run_in_front(
            &workspace,
            cx,
            "INSERT INTO other.main.t (name, id) VALUES ('second', 2)",
        );
        // `refresh_schema` reads `other`'s catalog again over its session.
        click_t_under(&workspace, cx, "other");
        let other = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        let Tab::Object(other_id) = other else {
            panic!("the table opened an object tab");
        };
        workspace.update(cx, |workspace, cx| workspace.count_rows(other_id, cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation {
                count, structure, ..
            } = &tab.body
            else {
                panic!("a relation tab");
            };
            assert!(
                matches!(count, crate::session::RowCount::Counted(_, 2)),
                "`other`'s two rows, not the connection's one"
            );
            let crate::StructureState::Loaded(structure) = structure else {
                panic!("the structure has not loaded");
            };
            let names: Vec<_> = structure.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                ["name", "id", "extra"],
                "`other`'s `t`, which has `extra`"
            );
        });

        click_t_under(&workspace, cx, "e2e");
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation { structure, .. } = &tab.body else {
                panic!("a relation tab");
            };
            let crate::StructureState::Loaded(structure) = structure else {
                panic!("the structure has not loaded");
            };
            let names: Vec<_> = structure.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                ["name", "id"],
                "the connection's own `t` has no `extra`"
            );
        });
        quit(workspace, cx);
    });
}

/// Runs `sql` in the profile's first query tab, which is its own and bound to
/// nothing, and reads the first cell of its answer. The tab in front is left
/// as it was.
fn answer_in_the_first_query_tab(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    sql: &str,
) -> Option<String> {
    let (front, first) = workspace.read_with(cx, |workspace, _| {
        let session = &workspace.profile().unwrap().session;
        (session.active, Tab::Query(session.queries[0].id))
    });
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.activate_tab(first, window, cx)
        });
    });
    settle(cx);
    run_in_front(workspace, cx, sql);
    let answered = answer(workspace, cx);
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.activate_tab(front, window, cx)
        });
    });
    settle(cx);
    answered
}

#[gpui::test]
fn an_edit_on_another_databases_table_is_written_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let grid = workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            match &tab.body {
                ObjectBody::Relation { results, .. } => results.clone(),
                _ => panic!("a relation tab"),
            }
        });
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().set_pending(
                0,
                0,
                crate::result_grid::NewValue::Value("edited".into())
            ));
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("edited".into()))),
            "the tab reads the edit back from the database it was written to"
        );
        assert_eq!(
            answer_in_the_first_query_tab(
                &workspace,
                cx,
                "SELECT name FROM other.main.t WHERE id = 1"
            )
            .as_deref(),
            Some("edited"),
            "`other.duckdb` changed"
        );
        assert_eq!(
            answer_in_the_first_query_tab(
                &workspace,
                cx,
                "SELECT name FROM e2e.main.t WHERE id = 1"
            )
            .as_deref(),
            Some("own"),
            "the connection's own file did not"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_export_of_another_databases_table_reads_it_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let path = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("exported.csv");
        workspace.update(cx, |workspace, cx| {
            let refetch = workspace
                .refetch()
                .expect("the tab's rows can be fetched again");
            let (id, generation) = {
                let profile = workspace.profile().unwrap();
                (profile.id.clone(), profile.generation)
            };
            workspace.start_export(
                &id,
                generation,
                refetch,
                crate::export::InsertTarget::default(),
                path.clone(),
                cx,
            );
        });
        for _ in 0..20 {
            settle(cx);
            if workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.exporting.is_none()
            }) {
                break;
            }
        }
        let written = std::fs::read_to_string(&path).expect("the export wrote its file");
        assert!(written.contains("other"), "{written}");
        assert!(!written.contains("own"), "{written}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_tab_of_another_database_comes_back_after_a_relaunch(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(
            saved.profiles[0]
                .open_objects
                .iter()
                .map(|object| object.database.as_deref())
                .collect::<Vec<_>>(),
            [Some("other")],
            "the database is written with its tab"
        );

        let (workspace, cx) = relaunch(workspace, cx);
        workspace.read_with(cx, |workspace, _| {
            let objects = &workspace.profile().unwrap().session.objects;
            assert_eq!(
                objects
                    .iter()
                    .map(|tab| tab.database.as_deref())
                    .collect::<Vec<_>>(),
                [Some("other")],
                "the tab is restored on its database"
            );
        });

        // `other` was attached to the first window's instance and is not on
        // the new one. Attached again, the tab reads it.
        front_the_tab(&workspace, cx, false);
        attach_other(&workspace, cx);
        let id = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.objects[0].id
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Object(id), window, cx);
                workspace.load_relation(id, cx);
            });
        });
        settle(cx);
        // Not the snapshot the tab was restored with: a run that completed.
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            assert!(
                matches!(
                    &tab.body,
                    ObjectBody::Relation {
                        query: QueryState::Complete { .. },
                        stale: false,
                        ..
                    }
                ),
                "the reload ran on `other`"
            );
        });
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into())))
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn select_database_closes_only_the_own_databases_object_tabs(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "e2e");
        click_t_under(&workspace, cx, "other");
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().session.objects.len(), 2);
        });

        // The picker is the simple mode's way to another database: the tabs
        // were opened in the connections tree, which only complex mode has.
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(crate::sidebar::ViewMode::Simple, cx)
        });
        settle(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_database("other".into(), window, cx)
            });
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let objects = &workspace.profile().unwrap().session.objects;
            assert_eq!(
                objects
                    .iter()
                    .map(|tab| tab.database.as_deref())
                    .collect::<Vec<_>>(),
                [Some("other")],
                "the connection's own tab went and the other database's stayed"
            );
        });

        // Its session was dropped with the switch's others and opens again
        // on the tab's next read.
        let id = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.objects[0].id
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Object(id), window, cx);
                workspace.requery_relation(id, |_, _, _, _| true, cx);
            });
        });
        settle(cx);
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into())))
        );
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            assert!(matches!(
                &tab.body,
                ObjectBody::Relation {
                    query: QueryState::Complete { .. },
                    ..
                }
            ));
        });
        quit(workspace, cx);
    });
}

/// `a` in front and `b` behind it, both connected and each on its one query
/// tab, over one SQLite file.
fn a_in_front_of_b<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    a_in_front_of_b_with(cx, settings, "")
}

/// `a_in_front_of_b`, `b` with `extra` among its scalar keys.
fn a_in_front_of_b_with<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        settings,
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", extra)
        ),
    );
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    activate_by_id(&workspace, cx, "b");
    activate_by_id(&workspace, cx, "a");
    (workspace, cx)
}

/// The strip's chips, left to right, by connection id.
fn strip_chips(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<(String, Tab)> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .strip_chips()
            .into_iter()
            .map(|(index, tab)| (workspace.profiles[index].id.clone(), tab))
            .collect()
    })
}

fn chip_part(
    cx: &mut VisualTestContext,
    part: &str,
    profile: &str,
    tab: Tab,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    // `debug_bounds` takes a `'static` selector; a test's few chips can leak.
    cx.debug_bounds(crate::views::chip_selector(part, profile, tab).leak())
}

fn new_query_tab(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.new_query(&crate::actions::NewQuery, window, cx);
        });
    });
    settle(cx);
}

#[gpui::test]
fn the_strip_in_complex_mode_lists_every_connections_tabs_and_a_foreign_chip_brings_its_connection_forward(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(
            chips.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "a", "b"],
            "a's two tabs, then b's one"
        );
        let lefts: Vec<_> = chips
            .iter()
            .map(|(id, tab)| {
                chip_part(cx, "", id, *tab)
                    .unwrap_or_else(|| panic!("the chip of {id} {tab:?} is not drawn"))
                    .left()
            })
            .collect();
        assert!(lefts.windows(2).all(|pair| pair[0] < pair[1]), "{lefts:?}");
        for (id, tab) in &chips {
            assert!(
                chip_part(cx, "context", id, *tab).is_some(),
                "the chip of {id} {tab:?} names its connection"
            );
        }
        workspace.read_with(cx, |workspace, _| {
            let context = |index: usize, tab: Tab| {
                crate::views::chip_context(&workspace.profiles[index], tab, true)
            };
            assert_eq!(context(1, chips[2].1).as_deref(), Some("Flocks"));
            assert_eq!(context(0, chips[0].1).as_deref(), Some("Birds"));
        });

        let (id, tab) = chips[2].clone();
        let at = chip_part(cx, "", &id, tab).unwrap().center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "b");
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().session.active, tab);
        });
        assert_eq!(
            strip_chips(&workspace, cx),
            chips,
            "a's tabs are still listed"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_bound_tabs_chip_names_its_connection_and_binding(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        let chips = strip_chips(&workspace, cx);
        let (bound, plain) = (chips[1].clone(), chips[0].clone());
        let context = |cx: &mut VisualTestContext, complex: bool| {
            workspace.read_with(cx, |workspace, _| {
                let profile = &workspace.profiles[0];
                (
                    crate::views::chip_context(profile, bound.1, complex),
                    crate::views::chip_context(profile, plain.1, complex),
                )
            })
        };
        assert_eq!(
            context(cx, true),
            (Some("E2E · other".into()), Some("E2E".into()))
        );
        assert!(chip_part(cx, "context", &bound.0, bound.1).is_some());
        assert!(chip_part(cx, "context", &plain.0, plain.1).is_some());

        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        assert_eq!(context(cx, false), (Some("other".into()), None));
        assert!(
            chip_part(cx, "context", &bound.0, bound.1).is_some(),
            "a bound chip says where it runs in simple mode"
        );
        assert!(
            chip_part(cx, "context", &plain.0, plain.1).is_none(),
            "an unbound chip says nothing in simple mode"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_strip_in_simple_mode_is_the_front_connections_alone(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(
            chips.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "a"]
        );
        for (id, tab) in &chips {
            assert!(chip_part(cx, "", id, *tab).is_some());
            assert!(chip_part(cx, "context", id, *tab).is_none());
        }
        let behind = workspace.read_with(cx, |workspace, _| {
            workspace.profiles[1].session.strip_tabs()
        });
        assert!(!behind.is_empty(), "b has a tab to leave out");
        for tab in behind {
            assert!(chip_part(cx, "", "b", tab).is_none());
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn only_the_front_connections_chips_can_be_dragged(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        let dragging = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.tab_strip.drag.is_some())
        };
        // Pressed and moved, not yet let go, which is when a drag is in hand.
        let pick_up = |cx: &mut VisualTestContext, id: &str, tab: Tab| {
            let from = chip_part(cx, "", id, tab)
                .expect("the chip is drawn")
                .center();
            let none = gpui::Modifiers::default();
            cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
            for at in [
                from + gpui::point(gpui::px(0.), gpui::px(8.)),
                from + gpui::point(gpui::px(12.), gpui::px(8.)),
            ] {
                cx.simulate_mouse_move(at, gpui::MouseButton::Left, none);
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            let held = dragging(cx);
            // Let go well below the chip: over it, the press would be a click
            // and bring its connection forward.
            let off = from + gpui::point(gpui::px(12.), gpui::px(300.));
            cx.simulate_mouse_move(off, gpui::MouseButton::Left, none);
            cx.simulate_mouse_up(off, gpui::MouseButton::Left, none);
            settle(cx);
            held
        };
        let chips = strip_chips(&workspace, cx);
        let (a, b) = (chips[0].clone(), chips[1].clone());
        assert_eq!((a.0.as_str(), b.0.as_str()), ("a", "b"));

        assert!(pick_up(cx, &a.0, a.1), "the front connection's chip drags");
        assert!(
            !pick_up(cx, &b.0, b.1),
            "another connection's chip is not dragged"
        );
        assert_eq!(front_profile(&workspace, cx), "a");

        let at = chip_part(cx, "", &b.0, b.1).unwrap().center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "b");
        assert!(pick_up(cx, &b.0, b.1), "once in front, its chip drags");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_coloured_connections_chips_follow_the_titlebar_setting(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            a_in_front_of_b_with(cx, "view_mode = \"complex\"", "color = \"red\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(chips.len(), 3);
        let tinted = |cx: &mut VisualTestContext| -> Vec<bool> {
            chips
                .iter()
                .map(|(id, tab)| chip_part(cx, "tint", id, *tab).is_some())
                .collect()
        };
        assert_eq!(tinted(cx), [false, false, true], "b is coloured, a is not");

        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(false, cx));
        settle(cx);
        assert_eq!(tinted(cx), [false, false, false]);

        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(true, cx));
        settle(cx);
        assert_eq!(tinted(cx), [false, false, true]);

        // The colour is complex mode's: simple mode draws b's chips as before.
        activate_by_id(&workspace, cx, "b");
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        let (id, tab) = chips[2].clone();
        assert!(chip_part(cx, "", &id, tab).is_some());
        assert!(chip_part(cx, "tint", &id, tab).is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_tab_chords_walk_every_connections_tabs_in_complex_mode(cx: &mut TestAppContext) {
    use crate::actions::{NextTab, PreviousTab};
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(chips.len(), 3);
        let at = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.profile().unwrap().id.clone(),
                    workspace.profile().unwrap().session.active,
                )
            })
        };
        let next = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| workspace.next_tab(&NextTab, window, cx));
            });
            settle(cx);
        };
        let previous = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.previous_tab(&PreviousTab, window, cx)
                });
            });
            settle(cx);
        };
        // a is in front on its last tab.
        assert_eq!(at(cx), chips[1]);
        next(cx);
        assert_eq!(at(cx), chips[2], "b's first tab, and b in front");
        // The end of the strip wraps to its start.
        next(cx);
        assert_eq!(at(cx), chips[0]);
        previous(cx);
        assert_eq!(at(cx), chips[2]);
        previous(cx);
        assert_eq!(at(cx), chips[1], "back on a");
        quit(workspace, cx);
    });
}

/// Right-click the connections tree's row `index`, step `downs` items down its
/// menu and take that one. The menu's labels are not readable from outside
/// the widget, so a menu is compared by what each of its items does.
fn pick_from_connections_menu(cx: &mut VisualTestContext, index: usize, downs: usize) {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-row-{index}").leak();
    let at = cx
        .debug_bounds(selector)
        .expect("the row is drawn")
        .center();
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        modifiers: gpui::Modifiers::default(),
        button: gpui::MouseButton::Right,
        click_count: 1,
        first_mouse: false,
    });
    settle(cx);
    cx.update(|window, _| {
        assert!(
            window
                .context_stack()
                .iter()
                .any(|context| context.contains("PopupMenu")),
            "the row's menu is up"
        )
    });
    for _ in 0..downs {
        cx.simulate_keystrokes("down");
    }
    press(cx, "enter");
}

#[gpui::test]
fn the_connections_trees_object_menu_is_simple_modes_menu(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("a", "Bïrds \"prod\"", ""),
                sqlite_profile("b", "Flocks", ""),
            ),
        );
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };
        // Connection `b` is not in front. Its table's menu is the one simple
        // mode offers: the same item does the same thing at the same place.
        let b = connection_row("b");
        let at = connections_row(&workspace, cx, &b).expect("b is listed");
        click_connections_row(cx, at);
        for id in [
            scoped_row("b", None, "schema-main"),
            scoped_row("b", None, "category-Tables-main"),
        ] {
            let at = connections_row(&workspace, cx, &id).expect("the row is showing");
            click_connections_row(cx, at);
        }
        let birds = scoped_row("b", None, &explorer::relation_row("main", "birds"));
        // Open Data, Open Structure, Import Rows, Export, then the copies:
        // Copy Name, Copy DDL, and Show DDL right after it.
        for (downs, copied, dialog) in [
            (5, "birds", None),
            (6, "CREATE TABLE birds (name text);", None),
            (7, "before menu", Some("show-ddl")),
        ] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
            let at = connections_row(&workspace, cx, &birds).expect("birds is showing");
            pick_from_connections_menu(cx, at, downs);
            assert_eq!(clipboard(cx), copied, "item {downs}");
            assert_eq!(front_dialog(&workspace, cx), dialog, "item {downs}");
            // Copying a name acts on nothing of the connection's.
            if downs > 5 {
                assert_eq!(
                    front_profile(&workspace, cx),
                    "b",
                    "the row's connection is in front once an item acts on it"
                );
            }
            if dialog.is_some() {
                press(cx, "escape");
            }
            // Back to `a`, so each item has to bring `b` forward itself.
            workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
            settle(cx);
        }

        // A connection's Copy Name is its name, whatever characters it holds.
        // New Query Here, Copy Name, Refresh.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
        let at = connections_row(&workspace, cx, &connection_row("a")).expect("a is listed");
        pick_from_connections_menu(cx, at, 2);
        assert_eq!(clipboard(cx), "Bïrds \"prod\"");

        quit(workspace, cx);
    });
}
