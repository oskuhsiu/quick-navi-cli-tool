use qnav::{
    app::App,
    model::*,
    scan::{ScanEvent, ScanEventKind},
};

fn app() -> App {
    App::new(
        vec![RootEntry {
            id: 1,
            path: "/work".into(),
        }],
        ExplorationPolicy { home: None },
    )
}
#[test]
fn tab_adopts_a_live_batch_without_dropping_the_selected_ancestry() {
    let mut app = app();
    let r = NodeKey::root(1);
    app.edit_query("docs".into());
    let req = app.prepare_scan(r.clone(), false).unwrap();
    app.accept_scan(ScanEvent {
        scope: req.scope,
        key: r.clone(),
        id: req.id,
        kind: ScanEventKind::Batch {
            names: vec!["docs".into()],
            examined: 1,
        },
    });
    app.match_tick();
    let wanted = Some(r.child("docs".as_ref()));
    assert_eq!(app.target(), wanted);
    app.clear_filter(true);
    assert_eq!(app.target(), wanted);
    assert!(!req.cancel.is_cancelled());
    assert!(app.prepare_scan(r, true).is_none());
}
#[test]
fn refresh_of_unselected_child_column_revalidates_parent_without_autoselect() {
    let mut app = app();
    app.move_vertical(true);
    app.move_horizontal(true);
    assert_eq!(app.target(), None);
    app.refresh_branch();
    assert!(app.restore.is_some());
    let r = NodeKey::root(1);
    let req = app.prepare_scan(r.clone(), true).unwrap();
    app.accept_scan(ScanEvent {
        scope: req.scope,
        key: r,
        id: req.id,
        kind: ScanEventKind::Done,
    });
    assert_eq!(app.target(), None);
    assert_eq!(app.browse_projection().focus, 1);
}
#[test]
fn paste_is_data_and_depth_never_changes() {
    let mut app = app();
    app.paste_query("repo[]");
    assert_eq!(app.depth, 3);
    assert_eq!(app.query(), "repo[]");
    app.paste_query("\n\u{3}");
    assert_eq!(app.query(), "repo[]");
}
#[test]
fn failed_ancestor_cannot_leave_a_valid_descendant_target() {
    let mut app = app();
    let r = NodeKey::root(1);
    app.edit_query("docs".into());
    let req = app.prepare_scan(r.clone(), false).unwrap();
    app.accept_scan(ScanEvent {
        scope: req.scope,
        key: r.clone(),
        id: req.id,
        kind: ScanEventKind::Batch {
            names: vec!["docs".into()],
            examined: 1,
        },
    });
    app.match_tick();
    assert!(app.target().is_some());
    app.accept_scan(ScanEvent {
        scope: req.scope,
        key: r,
        id: req.id,
        kind: ScanEventKind::Failed("gone".into()),
    });
    app.match_tick();
    assert!(app.target().is_none());
    assert_eq!(app.completion(), "partial");
}

#[test]
fn mkdir_excluded_child_restores_parent_not_a_ghost_target() {
    let mut app = app();
    app.move_vertical(true);
    app.freeze_action(ActionKind::Mkdir).unwrap();
    let request = app.pending_action.take().unwrap();
    app.directory_created(
        &request,
        std::path::Path::new("/work/.cache"),
        "created".into(),
    );
    assert_eq!(app.restore.as_ref().unwrap().key, NodeKey::root(1));
    assert!(app.status.contains("excluded"));
    assert_eq!(app.target(), None);
    let req = app.prepare_scan(NodeKey::root(1), true).unwrap();
    app.accept_scan(ScanEvent {
        scope: req.scope,
        key: req.key,
        id: req.id,
        kind: ScanEventKind::Done,
    });
    assert_eq!(app.target(), Some(NodeKey::root(1)));
    assert!(
        !app.nodes
            .contains_key(&NodeKey::root(1).child(".cache".as_ref()))
    );
}
