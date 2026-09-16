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
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
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
            "Ctrl+P pane",
            "Ctrl+C exit",
            "Ctrl+A add root",
            "Ctrl+N mkdir",
            "Ctrl+G git init",
            "Ctrl+R branch",
            "Ctrl+L all",
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
    terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
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
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
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
