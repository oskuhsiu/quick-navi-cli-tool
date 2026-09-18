use qnav::{app::App, model::*, ui};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};

#[test]
fn shortcut_help_is_bold_high_contrast_and_complete_in_narrow_panes() {
    let app = App::new(vec![], ExplorationPolicy { home: None });
    for (width, height) in [(120, 30), (80, 24), (40, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let footer = lines
            .iter()
            .skip_while(|line| !line.contains("↑↓ select"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        for command in [
            "Enter cd",
            "Ctrl+O hd",
            "Ctrl+B branch",
            "Ctrl+P pane",
            "Ctrl+C exit",
            "Ctrl+A add root",
            "Ctrl+N mkdir",
            "Ctrl+G git init",
            "Ctrl+R refresh branch",
            "Ctrl+L reload roots",
        ] {
            assert!(
                footer.lines().any(|line| line.contains(command)),
                "{width}x{height}: missing or split {command}\n{footer}"
            );
        }
        let help_cells = buffer
            .content()
            .iter()
            .filter(|cell| cell.bg == Color::Rgb(24, 29, 36) && !cell.symbol().trim().is_empty())
            .collect::<Vec<_>>();
        assert!(
            !help_cells.is_empty(),
            "shortcut help needs a distinct readable surface"
        );
        assert!(
            help_cells
                .iter()
                .all(|cell| cell.modifier.contains(Modifier::BOLD))
        );
        assert!(help_cells.iter().all(|cell| cell.fg != Color::DarkGray));
    }
}

#[test]
fn branch_picker_overlay_shows_worktree_state_and_blocks_background_hits() {
    let mut app = App::new(
        vec![RootEntry {
            id: 1,
            path: "/work/repo".into(),
        }],
        ExplorationPolicy { home: None },
    );
    app.move_vertical(true);
    let id = app.start_branch_picker().unwrap();
    app.set_branch_picker_entries(
        id,
        vec![
            BranchEntry {
                name: "main".into(),
                worktree: Some("/work/repo".into()),
                stale_worktree: false,
            },
            BranchEntry {
                name: "feature/demo".into(),
                worktree: None,
                stale_worktree: false,
            },
        ],
    );
    let (hits, terminal) = render_hits(&app, 100, 24);
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Branch workspace"));
    assert!(text.contains("feature/demo"));
    assert!(text.contains("new worktree"));
    assert!((0..24).all(|y| (0..100).all(|x| hits.hit(x, y).is_none())));
}

#[test]
fn partial_listing_is_visible_even_when_it_contains_rows() {
    let mut app = App::new(
        vec![RootEntry {
            id: 1,
            path: "/work".into(),
        }],
        ExplorationPolicy { home: None },
    );
    let root = NodeKey::root(1);
    let child = root.child("found".as_ref());
    app.nodes
        .insert(child.clone(), DirectoryNode::new(child.clone()));
    app.nodes
        .get_mut(&root)
        .unwrap()
        .children
        .insert("found".into(), child);
    app.nodes.get_mut(&root).unwrap().listing = ListingState::Partial("entry budget".into());
    app.move_vertical(true);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            ui::draw(frame, &app);
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("found"));
    assert!(text.to_lowercase().contains("partial"));
    assert!(text.contains("entry budget"));
}

#[test]
fn depth_nine_unicode_and_narrow_columns_render_without_a_second_target() {
    let mut app = App::new(
        vec![RootEntry {
            id: 1,
            path: "/work".into(),
        }],
        ExplorationPolicy { home: None },
    );
    for _ in 0..20 {
        app.change_depth(true);
    }
    assert_eq!(app.depth, 9);
    let mut parent = NodeKey::root(1);
    for n in 0..9 {
        let name = format!("目录{n}-e\u{301}");
        let key = parent.child(name.as_ref());
        app.nodes
            .get_mut(&parent)
            .unwrap()
            .children
            .insert(name.into(), key.clone());
        app.nodes.get_mut(&parent).unwrap().listing = ListingState::Ready;
        app.nodes
            .insert(key.clone(), DirectoryNode::new(key.clone()));
        parent = key;
    }
    app.navigation = Navigation::Browse(BrowseState::at(&parent));
    assert!(app.prepare_scan(parent.clone(), true).is_none());
    for (width, height) in [(120, 30), (80, 24), (40, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &app);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        // TestBackend retains a blank continuation cell for each double-width glyph.
        assert!(
            text.replace(' ', "").contains("目录8"),
            "{width}x{height}: {text}"
        );
        assert!(text.contains('‹'));
        assert_eq!(app.target(), Some(parent.clone()));
    }
    for _ in 0..20 {
        app.change_depth(false);
    }
    assert_eq!(app.depth, 1);
}

fn render_hits(app: &App, width: u16, height: u16) -> (ui::HitMap, Terminal<TestBackend>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hits = ui::HitMap::default();
    terminal.draw(|frame| hits = ui::draw(frame, app)).unwrap();
    (hits, terminal)
}

fn many_roots() -> App {
    App::new(
        (1..=40)
            .map(|id| RootEntry {
                id,
                path: format!("/root{id:02}").into(),
            })
            .collect(),
        ExplorationPolicy { home: None },
    )
}

#[test]
fn column_hits_follow_visible_rows_and_keep_rendered_identity() {
    let mut app = many_roots();
    app.navigation = Navigation::Browse(BrowseState::at(&NodeKey::root(30)));
    let (hits, terminal) = render_hits(&app, 80, 24);
    let buffer = terminal.backend().buffer();
    let mut row_count = 0;
    for y in 0..24 {
        if let Some((ui::MouseList::Column(0), Some(key))) = hits.hit(1, y) {
            let line = (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(line.contains(&format!("root{:02}", key.root)), "{line}");
            row_count += 1;
        }
    }
    assert!(row_count > 1 && row_count < 40);
    let first = hits.hit(1, 2).unwrap().1.unwrap().clone();
    assert!(first.root > 1, "selection should scroll the first row");
    app.roots.reverse();
    assert_eq!(
        hits.hit(1, 2),
        Some((ui::MouseList::Column(0), Some(&first)))
    );
    assert_eq!(hits.hit(0, 2), None);
    assert_eq!(hits.hit(1, 1), None);
    assert_eq!(hits.hit(1, 0), None);
    assert_eq!(hits.hit(1, 23), None);
}

#[test]
fn narrow_deep_column_hits_use_absolute_column_index() {
    let mut app = many_roots();
    app.depth = 9;
    let key = NodeKey::root(1);
    let child = key.child("child".as_ref());
    app.nodes
        .get_mut(&key)
        .unwrap()
        .children
        .insert("child".into(), child.clone());
    app.nodes
        .insert(child.clone(), DirectoryNode::new(child.clone()));
    app.navigation = Navigation::Browse(BrowseState::at(&child));
    let (hits, _) = render_hits(&app, 40, 12);
    assert_eq!(
        hits.hit(1, 2),
        Some((ui::MouseList::Column(1), Some(&child)))
    );
    assert_eq!(hits.hit(1, 3), Some((ui::MouseList::Column(1), None)));
    assert_eq!(hits.hit(39, 2), None);
}

#[test]
fn match_hits_follow_scrolled_rows_and_ancestry_is_read_only() {
    let mut app = many_roots();
    app.navigation = Navigation::Filter(SearchState {
        query: "root".into(),
        revision: 1,
        matches: (1..=40).map(NodeKey::root).collect(),
        selection: MatchSelection::Selected(NodeKey::root(30)),
        snapshot: BrowseState::default(),
        viewport: 0,
    });
    let (hits, terminal) = render_hits(&app, 80, 24);
    let buffer = terminal.backend().buffer();
    let mut seen = Vec::new();
    for y in 0..24 {
        if let Some((list, Some(key))) = hits.hit(1, y) {
            assert_eq!(list, ui::MouseList::Matches);
            let line = (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(line.contains(&format!("root{:02}", key.root)), "{line}");
            seen.push(key.root);
        }
    }
    assert!(seen.len() > 1 && seen[0] > 1);
    assert!(seen.contains(&30));
    assert_eq!(hits.hit(1, 2), None);
}

#[test]
fn empty_space_small_screens_and_modal_overlays_have_no_row_hits() {
    let mut app = App::new(vec![], ExplorationPolicy { home: None });
    let (hits, _) = render_hits(&app, 80, 24);
    assert_eq!(hits.hit(1, 2), Some((ui::MouseList::Column(0), None)));
    app = many_roots();
    for (width, height) in [(23, 24), (80, 7)] {
        let (hits, _) = render_hits(&app, width, height);
        assert!((0..height).all(|y| (0..width).all(|x| hits.hit(x, y).is_none())));
    }
    app.add_root = Some(String::new());
    let (hits, _) = render_hits(&app, 80, 24);
    assert!((0..24).all(|y| (0..80).all(|x| hits.hit(x, y).is_none())));
    app.add_root = None;
    for phase in [
        ActionPhase::Input(String::new()),
        ActionPhase::Confirm {
            message: "continue?".into(),
            accepted: false,
        },
    ] {
        app.pending_action = Some(ActionRequest {
            id: 1,
            kind: ActionKind::Mkdir,
            source: NodeKey::root(1),
            target: "/root01".into(),
            phase,
            child_name: None,
            branch: None,
            cancel: CancelToken::default(),
        });
        let (hits, _) = render_hits(&app, 80, 24);
        assert!((0..24).all(|y| (0..80).all(|x| hits.hit(x, y).is_none())));
    }
}
