//! The app driven the way a person drives it: launched from a saved
//! `profiles.toml`, typed into, run from the keyboard. GPUI's test platform
//! has no screen, so these run headless in CI like any other test.

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, Window,
};
use gpui_component::{Root, input::InputState};

use super::Workspace;
use crate::{
    actions::{CancelQuery, ImportRowsFromClipboard},
    connection_form::ConnectionForm,
    db::{CancelToken, ConnectionConfig, Engine, ExternalConfig, Fetch, SshTunnel},
    explain::analysis::Basis,
    explorer, keybindings,
    palette::{Command, Mode as PaletteMode, Scope},
    session::{CatalogState, ObjectBody, QueryState, QueryTab, Tab},
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
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.db");
    std::fs::write(&database, b"").expect("the database file must be creatable");
    std::fs::write(
        directory.join("profiles.toml"),
        format!("[settings]\ncheck_for_updates = false\n{settings}\n{profiles}")
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
            workspace.open_import_file(profile, "main".into(), "birds".into(), file, window, cx);
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
            workspace.import_rows_into("main".into(), "birds".into(), window, cx);
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
                let member = workspace.member_at(leaf.target);
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
                let birds = workspace.member_at(leaf.target).unwrap();
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
                    let member = workspace.member_at(leaf.target).unwrap();
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
                let id = workspace.open_object(opened, window, cx).unwrap();
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
                let birds = workspace.member_at(leaf.target).unwrap();
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
        assert_ne!(
            notice(&workspace, cx).as_deref(),
            Some("Renamed is hidden from agents because its engine changed.")
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
        let client = raw_call(&path, r#"{"v":1,"kind":"call","tool":"list_connections"}"#);
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
        std::io::Write::write_all(&mut stream, b"{\"v\":1,\"kind\":\"presence\"}\n")
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
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = crate::agent::CallId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let (reply, answers) = std::sync::mpsc::sync_channel(1);
    let call = crate::agent::Inbound::Call {
        id,
        tool,
        connection: connection.map(str::to_string),
        reply,
    };
    workspace.update(cx, |workspace, cx| workspace.on_agent_message(call, cx));
    (id, answers)
}

fn pending_calls(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<crate::agent::CallId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.agent.calls.iter().map(|call| call.id).collect()
    })
}

#[gpui::test]
fn a_ping_for_an_unknown_or_hidden_connection_is_refused_with_the_lookup_text(
    cx: &mut TestAppContext,
) {
    use crate::agent::{Reply, Status, tools::Tool};

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
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
            let (_, answers) = inject_call(&workspace, cx, Tool::Ping, name);
            assert_eq!(
                answers.try_recv().ok(),
                Some(refused("no agent-visible connection has that name")),
                "{name:?}"
            );
        }
        let (_, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Dup"));
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
fn an_unanswered_ping_expires_after_the_fixed_wait(cx: &mut TestAppContext) {
    use std::{sync::mpsc::TryRecvError, time::Duration};

    use crate::agent::{EXPIRY, Reply, Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        let (id, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
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
                text: "Nobody answered the ping in DBDelve within 5 minutes.".to_string(),
            })
        );
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(answers.try_recv().is_err(), "nothing else is ever sent");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn hanging_up_one_ping_cancels_only_that_ping(cx: &mut TestAppContext) {
    use std::{
        io::{ErrorKind, Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

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
        let ping = b"{\"v\":1,\"kind\":\"call\",\"tool\":\"ping\",\"connection\":\"Orders\"}\n";
        let connect = || {
            let mut stream = UnixStream::connect(&path).expect("the socket accepts a client");
            stream.write_all(ping).expect("the frame is written");
            stream
        };

        let first = connect();
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        let first_id = pending_calls(&workspace, cx)[0];
        let mut second = connect();
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 2);
        let second_id = pending_calls(&workspace, cx)[1];
        assert_ne!(first_id, second_id);

        drop(first);
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        assert_eq!(pending_calls(&workspace, cx), [second_id]);
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

/// The labels of the palette's agent access row, which is one row or none.
#[cfg(unix)]
fn agent_access_rows(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        crate::palette::agent_items(workspace)
            .into_iter()
            .map(|item| item.label)
            .collect()
    })
}

#[cfg(unix)]
const PING_ORDERS: &str = r#"{"v":1,"kind":"call","tool":"ping","connection":"Orders"}"#;

#[cfg(unix)]
#[gpui::test]
fn turning_agent_access_on_listens_and_turning_it_off_leaves_nothing_listening(
    cx: &mut TestAppContext,
) {
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};

    use crate::agent::{Reply, Status};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
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

        let listed = raw_call(&path, r#"{"v":1,"kind":"call","tool":"list_connections"}"#);
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

        let ping = raw_call(&path, PING_ORDERS);
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);

        run_command(&workspace, cx, Command::DisconnectAgents);
        drive_until(cx, |_| ping.is_finished());
        assert_eq!(
            ping.join().expect("the client thread finished").trim_end(),
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
const PRESENCE: &str = r#"{"v":1,"kind":"presence"}"#;

#[cfg(unix)]
const REJECTED: &str = r#"{"status":"rejected","text":"Agent access was turned off in DBDelve."}"#;

#[cfg(unix)]
#[gpui::test]
fn the_kill_switch_rejects_every_pending_call_before_it_closes_the_socket(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");

        let pings = [raw_call(&path, PING_ORDERS), raw_call(&path, PING_ORDERS)];
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 2);
        let presence = raw_call(&path, PRESENCE);
        drive_until(cx, |cx| {
            workspace.read_with(cx, |workspace, _| workspace.agent.presence.len() == 1)
        });
        // A waiting ping's dialog holds the keyboard, so the chord is pressed
        // from inside it, and has to reach the workspace from there.
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert!(dialog_has_keyboard(&workspace, cx));

        cx.simulate_keystrokes("secondary-shift-k");
        drive_until(cx, |_| {
            pings.iter().all(|ping| ping.is_finished()) && presence.is_finished()
        });
        for ping in pings {
            // A whole frame, then the end of the connection: a bare end of
            // file would read as an empty string.
            assert_eq!(
                ping.join().expect("the client thread finished"),
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

/// The call the ping dialog is about.
fn front_ping_id(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::agent::CallId> {
    workspace.read_with(cx, |workspace, _| {
        workspace.front_ping().map(|call| call.id)
    })
}

fn denied() -> crate::agent::Reply {
    crate::agent::Reply {
        status: crate::agent::Status::Rejected,
        text: "The ping was denied in DBDelve.".to_string(),
    }
}

#[cfg(unix)]
#[gpui::test]
fn a_ping_through_the_shim_waits_for_the_human_and_answers_executed(cx: &mut TestAppContext) {
    use crate::session::ProfileState;

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("orders", "Orders", visible),
                sqlite_profile("reports", "Reports", visible),
            ]
            .concat(),
        );
        cx.executor().allow_parking();
        run_command(&workspace, cx, Command::TurnOnAgentAccess);
        let path = crate::agent::socket_path().expect("the test home gives a socket path");

        let shim = crate::agent::shim::testing::call_through_the_shim(
            path,
            "ping",
            serde_json::json!({ "connection": "Reports" }),
        );
        drive_until(cx, |cx| pending_calls(&workspace, cx).len() == 1);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert_eq!(ringed_button(&workspace, cx), 0);
        assert!(!shim.is_finished(), "the agent waits for the human");

        let approve = cx
            .debug_bounds("agent-ping-approve")
            .expect("the dialog draws Approve");
        cx.simulate_click(approve.center(), gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);

        drive_until(cx, |_| shim.is_finished());
        let result = shim.join().expect("the shim's thread ends");
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(
            result["content"][0]["text"],
            "status: executed\nThe ping was approved in DBDelve."
        );
        workspace.read_with(cx, |workspace, _| {
            let reports = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "reports")
                .expect("the profile was restored");
            assert!(
                matches!(reports.state, ProfileState::Idle),
                "approving a ping opened a connection"
            );
        });
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn enter_never_approves_a_ping(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    use crate::agent::tools::Tool;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        let (id, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert_eq!(ringed_button(&workspace, cx), 0);

        for key in ["right", "left", "tab"] {
            press(cx, key);
            assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"), "{key}");
            assert_eq!(ringed_button(&workspace, cx), 0, "{key}");
            assert_eq!(pending_calls(&workspace, cx), [id], "{key}");
            assert_eq!(answers.try_recv().err(), Some(TryRecvError::Empty), "{key}");
        }

        press(cx, "enter");
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(pending_calls(&workspace, cx).is_empty());
        assert!(editor_has_keyboard(&workspace, cx));

        let (_, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        press(cx, "escape");
        assert_eq!(answers.try_recv().ok(), Some(denied()));
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_ping_that_ends_elsewhere_closes_its_dialog_and_hands_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    use crate::agent::{EXPIRY, Inbound, Reply, Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );

        // Hung up.
        let (id, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
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
        let (_, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
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
        let (_, answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
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
        quit(workspace, cx);
    });
}

#[gpui::test]
fn pings_are_answered_oldest_first(cx: &mut TestAppContext) {
    use std::sync::mpsc::TryRecvError;

    use crate::agent::{Reply, Status, tools::Tool};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_with_profiles(
            cx,
            &sqlite_profile("orders", "Orders", "agent_visible = true"),
        );
        let (first, first_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        let (second, second_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert_eq!(front_ping_id(&workspace, cx), Some(first));

        workspace.update(cx, |workspace, cx| workspace.approve_ping(first, cx));
        settle(cx);
        assert_eq!(
            first_answers.try_recv().ok(),
            Some(Reply {
                status: Status::Executed,
                text: "The ping was approved in DBDelve.".to_string(),
            })
        );
        assert_eq!(pending_calls(&workspace, cx), [second]);
        assert_eq!(front_ping_id(&workspace, cx), Some(second));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert_eq!(
            second_answers.try_recv().err(),
            Some(TryRecvError::Empty),
            "the second ping was answered by the first's approval"
        );
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

    use crate::agent::tools::Tool;

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
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
        let (first_shop, first_shop_answers) =
            inject_call(&workspace, cx, Tool::Ping, Some("Shop"));
        let (orders, orders_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        let (second_shop, second_shop_answers) =
            inject_call(&workspace, cx, Tool::Ping, Some("Shop"));
        let (reports, reports_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Reports"));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        assert_eq!(front_ping_id(&workspace, cx), Some(first_shop));

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
        assert_eq!(front_ping_id(&workspace, cx), Some(orders));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));

        // Nothing waits on Reports once its ping is denied, so hiding it sends
        // nothing and says nothing.
        workspace.update(cx, |workspace, cx| workspace.deny_ping(orders, cx));
        workspace.update(cx, |workspace, cx| workspace.deny_ping(reports, cx));
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

    use crate::agent::tools::Tool;

    store::tests::with_home(|| {
        let visible = "agent_visible = true";
        let (workspace, cx) = launch_with_profiles(
            cx,
            &[
                sqlite_profile("o", "Orders", visible),
                sqlite_profile("r", "Reports", visible),
            ]
            .concat(),
        );
        let (orders, orders_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Orders"));
        let (reports, reports_answers) = inject_call(&workspace, cx, Tool::Ping, Some("Reports"));
        settle(cx);
        assert_eq!(front_ping_id(&workspace, cx), Some(orders));

        workspace.update(cx, |workspace, cx| workspace.remove_profile(0, cx));
        settle(cx);
        assert_eq!(orders_answers.try_recv().ok(), Some(swept()));
        assert_eq!(pending_calls(&workspace, cx), [reports]);
        assert_eq!(reports_answers.try_recv().err(), Some(TryRecvError::Empty));
        assert_eq!(front_ping_id(&workspace, cx), Some(reports));
        assert_eq!(front_dialog(&workspace, cx), Some("agent-ping"));
        quit(workspace, cx);
    });
}
