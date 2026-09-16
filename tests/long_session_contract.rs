use qnav::{app::App, model::*, scan::Scanner};
use std::time::{Duration, Instant};

fn settle(app: &mut App, scanner: &Scanner) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        app.schedule(scanner);
        while let Some(event) = scanner.try_recv() {
            app.accept_scan(event);
        }
        app.match_tick();
        if app.completion() == "complete" && !app.working() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "search did not finish: {}",
            app.completion()
        );
        std::thread::yield_now();
    }
}

#[test]
fn one_session_survives_100_scope_refresh_and_search_cycles_without_growth() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let roots: Vec<_> = (1..=2)
        .map(|id| {
            let path = base.join(format!("root{id}"));
            for a in 0..5 {
                for b in 0..5 {
                    std::fs::create_dir_all(path.join(format!("project{a}/docs{b}"))).unwrap();
                }
            }
            RootEntry { id, path }
        })
        .collect();
    let policy = ExplorationPolicy { home: None };
    let scanner = Scanner::new(policy.clone());
    let mut app = App::new(roots.clone(), policy);
    for cycle in 0..100 {
        app.change_depth(true);
        app.edit_query("project".into());
        settle(&mut app, &scanner);
        assert_eq!(app.nodes.len(), 62);
        app.refresh_branch();
        settle(&mut app, &scanner);
        app.clear_filter(true);
        assert!(app.target().is_some());
        app.change_depth(false);
        app.edit_query("project".into());
        settle(&mut app, &scanner);
        app.reload_roots(roots.clone());
        settle(&mut app, &scanner);
        assert_eq!(app.nodes.len(), 12, "cycle {cycle} accumulated stale nodes");
        assert_eq!(app.pending_scan_count(), 0);
        for (key, node) in &app.nodes {
            if let Some(parent) = key.parent() {
                assert!(app.nodes.contains_key(&parent));
            }
            for child in node.children.values() {
                assert!(app.nodes.contains_key(child));
            }
        }
        if let Navigation::Filter(search) = &app.navigation {
            assert!(search.matches.iter().all(|k| app.valid(k)));
        }
        if let Some(target) = app.target() {
            assert!(app.valid(&target));
        }
        app.clear_filter(false);
    }
}
