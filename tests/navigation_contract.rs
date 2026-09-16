use qnav::{
    app::App,
    model::*,
    scan::{ScanEvent, ScanEventKind, ScanRequest},
};
use std::{ffi::OsStr, path::PathBuf};

fn app(paths: &[&str]) -> App {
    App::new(
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| RootEntry {
                id: i as u64 + 1,
                path: p.into(),
            })
            .collect(),
        ExplorationPolicy { home: None },
    )
}
fn child(parent: &NodeKey, name: &str) -> NodeKey {
    parent.child(OsStr::new(name))
}
fn event(request: &ScanRequest, kind: ScanEventKind) -> ScanEvent {
    ScanEvent {
        scope: request.scope,
        key: request.key.clone(),
        id: request.id,
        kind,
    }
}
fn batch(app: &mut App, request: &ScanRequest, names: &[&str]) {
    assert!(app.accept_scan(event(
        request,
        ScanEventKind::Batch {
            names: names.iter().map(|n| (*n).into()).collect(),
            examined: names.len()
        }
    )));
}
fn listing(app: &mut App, key: &NodeKey, names: &[&str]) {
    let request = app
        .prepare_scan(key.clone(), false)
        .expect("listing must be schedulable");
    batch(app, &request, names);
    assert!(app.accept_scan(event(&request, ScanEventKind::Done)));
}
fn match_all(app: &mut App) {
    for _ in 0..100 {
        app.match_tick();
    }
}

#[test]
fn startup_and_unselected_child_column_have_no_target() {
    let mut app = app(&["/work", "/other"]);
    let root = NodeKey::root(1);
    assert_eq!(app.depth, 1);
    assert_eq!(app.target(), None);
    assert_eq!(app.pending_scan_count(), 0);
    assert!(
        app.nodes
            .values()
            .all(|n| n.children.is_empty() && n.listing == ListingState::Unread)
    );
    app.move_vertical(true);
    assert_eq!(app.target(), Some(root.clone()));
    listing(&mut app, &root, &["docs"]);
    assert_eq!(app.browse_projection().chain, vec![root.clone()]);
    app.move_horizontal(true);
    assert_eq!(app.target(), None);
    assert!(app.freeze_action(ActionKind::Mkdir).is_none());
    app.move_horizontal(false);
    assert_eq!(app.target(), Some(root));
}

#[test]
fn changing_parent_and_shrinking_clear_old_descendant_selection() {
    let mut app = app(&["/work", "/other"]);
    app.change_depth(true);
    let root = NodeKey::root(1);
    let a = child(&root, "a");
    let deep = child(&a, "deep");
    listing(&mut app, &root, &["a"]);
    listing(&mut app, &a, &["deep"]);
    app.navigation = Navigation::Browse(BrowseState::at(&deep));
    app.change_depth(false);
    assert_eq!(app.target(), Some(a.clone()));
    app.change_depth(true);
    assert_eq!(app.target(), Some(a));
    app.move_horizontal(false);
    app.move_vertical(true);
    assert_eq!(app.target(), Some(NodeKey::root(2)));
    assert_eq!(app.browse_projection().chain, vec![NodeKey::root(2)]);
}

#[test]
fn filter_discovers_unvisited_branches_and_keeps_same_query_selection() {
    let mut app = app(&["/work"]);
    let root = NodeKey::root(1);
    app.edit_query("n".into());
    let request = app.prepare_scan(root.clone(), false).unwrap();
    batch(&mut app, &request, &["navigation"]);
    match_all(&mut app);
    assert_eq!(app.target(), Some(child(&root, "navigation")));
    batch(&mut app, &request, &["n", "nav"]);
    match_all(&mut app);
    assert_eq!(app.target(), Some(child(&root, "navigation")));
    let pending = app.pending_scan_count();
    app.edit_query("nav".into());
    match_all(&mut app);
    assert_eq!(app.target(), Some(child(&root, "nav")));
    assert_eq!(
        app.pending_scan_count(),
        pending,
        "query edits do not rescan"
    );
    assert!(app.accept_scan(event(&request, ScanEventKind::Done)));
}

#[test]
fn tab_adopts_match_while_escape_restores_snapshot() {
    let mut app = app(&["/work"]);
    let root = NodeKey::root(1);
    listing(&mut app, &root, &["docs"]);
    app.move_vertical(true);
    app.edit_query("docs".into());
    match_all(&mut app);
    app.clear_filter(false);
    assert_eq!(app.target(), Some(root.clone()));
    app.edit_query("docs".into());
    match_all(&mut app);
    app.clear_filter(true);
    assert!(matches!(app.navigation, Navigation::Browse(_)));
    assert_eq!(app.target(), Some(child(&root, "docs")));
    assert!(
        app.prepare_scan(child(&root, "docs"), true).is_none(),
        "depth boundary must not list children"
    );
}

#[test]
fn cancelled_filter_work_cannot_overwrite_new_listing() {
    let mut app = app(&["/work"]);
    let root = NodeKey::root(1);
    app.edit_query("a".into());
    let old = app.prepare_scan(root.clone(), false).unwrap();
    batch(&mut app, &old, &["abandoned"]);
    app.clear_filter(false);
    assert!(old.cancel.is_cancelled());
    let new = app.prepare_scan(root.clone(), true).unwrap();
    assert!(new.id > old.id);
    for kind in [
        ScanEventKind::Batch {
            names: vec!["stale".into()],
            examined: 1,
        },
        ScanEventKind::Done,
        ScanEventKind::Failed("old".into()),
    ] {
        assert!(!app.accept_scan(event(&old, kind)));
    }
    batch(&mut app, &new, &["fresh"]);
    assert!(app.accept_scan(event(&new, ScanEventKind::Done)));
    assert_eq!(
        app.nodes[&root]
            .children
            .values()
            .cloned()
            .collect::<Vec<_>>(),
        vec![child(&root, "fresh")]
    );
}

#[test]
fn branch_refresh_uses_focused_parent_and_invalidates_overlapping_occurrences() {
    let mut app = app(&["/work", "/work/a", "/other"]);
    app.change_depth(true);
    let root = NodeKey::root(1);
    let a = child(&root, "a");
    let overlap = NodeKey::root(2);
    let other = NodeKey::root(3);
    listing(&mut app, &root, &["a", "sibling"]);
    listing(&mut app, &a, &["docs"]);
    listing(&mut app, &overlap, &["docs"]);
    listing(&mut app, &other, &["safe"]);
    app.navigation = Navigation::Browse(BrowseState::at(&child(&a, "docs")));
    assert_eq!(app.branch(), Some(PathBuf::from("/work/a")));
    let scope = app.scope;
    app.refresh_branch();
    assert_eq!(app.scope, scope);
    assert!(!app.nodes.contains_key(&child(&a, "docs")));
    assert!(!app.nodes.contains_key(&child(&overlap, "docs")));
    assert_eq!(app.nodes[&a].listing, ListingState::Unread);
    assert_eq!(app.nodes[&overlap].listing, ListingState::Unread);
    assert_eq!(app.nodes[&other].listing, ListingState::Ready);
    assert!(app.nodes.contains_key(&child(&other, "safe")));
    assert!(app.nodes.contains_key(&child(&root, "sibling")));
    assert_eq!(app.target(), None);
}

#[test]
fn full_reload_same_id_with_changed_path_cannot_restore_same_named_child() {
    let mut app = app(&["/old"]);
    let root = NodeKey::root(1);
    listing(&mut app, &root, &["docs"]);
    app.navigation = Navigation::Browse(BrowseState::at(&child(&root, "docs")));
    app.reload_roots(vec![RootEntry {
        id: 1,
        path: "/new".into(),
    }]);
    assert!(app.restore.is_none());
    assert_eq!(app.target(), None);
    listing(&mut app, &root, &["docs"]);
    assert_eq!(app.target(), None);
    assert!(app.browse_projection().chain.is_empty());
}

#[test]
fn active_filter_pins_nonmatches_and_capacity_stays_partial() {
    let mut app = app(&["/work"]);
    app.max_nodes = 2;
    let root = NodeKey::root(1);
    app.edit_query("alpha".into());
    let request = app.prepare_scan(root.clone(), false).unwrap();
    batch(&mut app, &request, &["alpha", "beta", "gamma"]);
    assert!(request.cancel.is_cancelled());
    assert!(app.nodes.contains_key(&child(&root, "beta")));
    assert!(!app.nodes.contains_key(&child(&root, "gamma")));
    assert_eq!(app.completion(), "partial");
    assert!(!app.accept_scan(event(&request, ScanEventKind::Done)));
    app.edit_query("beta".into());
    match_all(&mut app);
    assert_eq!(app.target(), Some(child(&root, "beta")));
    assert_eq!(app.completion(), "partial");
}

#[test]
fn checking_action_freezes_path_and_cancellation_keeps_scope_locked() {
    let mut app = app(&["/first", "/second"]);
    app.move_vertical(true);
    let id = app.freeze_action(ActionKind::GitInit).unwrap();
    app.move_vertical(true);
    assert_eq!(app.target(), Some(NodeKey::root(2)));
    assert_eq!(
        app.pending_action.as_ref().unwrap().target,
        PathBuf::from("/first")
    );
    let scope = app.scope;
    app.change_depth(true);
    app.refresh_branch();
    app.reload_roots(vec![]);
    assert_eq!(app.depth, 1);
    assert_eq!(app.scope, scope);
    assert_eq!(app.roots.len(), 2);
    assert!(app.freeze_action(ActionKind::Mkdir).is_none());
    app.cancel_action();
    let pending = app.pending_action.as_ref().unwrap();
    assert_eq!(pending.id, id);
    assert!(pending.cancel.is_cancelled());
    assert_eq!(pending.phase, ActionPhase::Cancelling);
    assert!(app.scope_locked());
    assert!(app.freeze_action(ActionKind::OpenPane).is_none());
}
