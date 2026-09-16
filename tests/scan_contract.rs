use qnav::{
    model::{CancelToken, ExplorationPolicy, NodeKey},
    scan::{ScanEventKind, ScanRequest, Scanner, validate_directory},
};
use std::{
    ffi::OsString,
    path::Path,
    sync::{Arc, atomic::AtomicUsize},
    time::{Duration, Instant},
};

fn request(path: &Path, budget: usize) -> ScanRequest {
    ScanRequest {
        scope: 1,
        key: NodeKey::root(1),
        id: 1,
        root_path: path.into(),
        path: path.into(),
        cancel: CancelToken::default(),
        entry_budget: Arc::new(AtomicUsize::new(budget)),
        list_children: true,
    }
}
fn collect(scanner: &Scanner) -> Vec<ScanEventKind> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    loop {
        if let Some(event) = scanner.try_recv() {
            let terminal = !matches!(event.kind, ScanEventKind::Batch { .. });
            events.push(event.kind);
            if terminal {
                return events;
            }
        }
        assert!(Instant::now() < deadline, "scanner timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn filters_files_symlinks_and_pruned_names_but_keeps_hidden_directories() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    for name in [".codex", ".local", "visible", "build", ".git"] {
        std::fs::create_dir(path.join(name)).unwrap();
    }
    std::fs::write(path.join("file"), "").unwrap();
    std::os::unix::fs::symlink(path.join("visible"), path.join("link")).unwrap();
    let scanner = Scanner::new(ExplorationPolicy { home: None });
    scanner.try_submit(request(&path, 100), true).unwrap();
    let events = collect(&scanner);
    assert!(matches!(events.last(), Some(ScanEventKind::Done)));
    let mut names: Vec<OsString> = events
        .into_iter()
        .flat_map(|event| match event {
            ScanEventKind::Batch { names, .. } => names,
            _ => vec![],
        })
        .collect();
    names.sort();
    assert_eq!(names, [".codex", ".local", "visible"].map(OsString::from));
    scanner
        .try_submit(request(&path.join("build"), 100), true)
        .unwrap();
    assert!(matches!(
        collect(&scanner).last(),
        Some(ScanEventKind::Done)
    ));
}
#[test]
fn ignored_entries_are_batched_and_consume_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    for n in 0..700 {
        std::fs::write(path.join(n.to_string()), "").unwrap();
    }
    let scanner = Scanner::new(ExplorationPolicy { home: None });
    scanner.try_submit(request(&path, 600), false).unwrap();
    let events = collect(&scanner);
    assert!(matches!(events.last(), Some(ScanEventKind::Partial(_))));
    let examined: usize = events
        .iter()
        .map(|event| match event {
            ScanEventKind::Batch { names, examined } => {
                assert!(names.is_empty());
                assert!(*examined <= 256);
                *examined
            }
            _ => 0,
        })
        .sum();
    assert_eq!(examined, 600);
}
#[test]
fn rejects_symlink_ancestors_and_cancelled_submission() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    std::fs::create_dir_all(path.join("real/child")).unwrap();
    std::os::unix::fs::symlink(path.join("real"), path.join("link")).unwrap();
    assert!(validate_directory(&path.join("link/child")).is_err());
    let scanner = Scanner::new(ExplorationPolicy { home: None });
    let req = request(&path, 10);
    req.cancel.cancel();
    assert!(scanner.try_submit(req, true).is_err());
}

#[test]
fn queued_replaced_directory_is_not_followed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    std::fs::create_dir(path.join("busy")).unwrap();
    std::fs::create_dir(path.join("queued")).unwrap();
    std::fs::create_dir(path.join("outside")).unwrap();
    for n in 0..3000 {
        std::fs::write(path.join("busy").join(n.to_string()), "").unwrap();
    }
    let scanner = Scanner::new(ExplorationPolicy { home: None });
    let first = request(&path.join("busy"), 4000);
    let cancel = first.cancel.clone();
    scanner.try_submit(first, false).unwrap();
    let second = request(&path.join("queued"), 10);
    scanner.try_submit(second, false).unwrap();
    // The first listing cannot finish while its more than eight batches are
    // undrained, so the background lane must eventually reject excess work.
    let rejected = (0..40)
        .filter(|_| {
            scanner
                .try_submit(request(&path.join("queued"), 10), false)
                .is_err()
        })
        .count();
    assert!(rejected >= 8);
    std::fs::rename(path.join("queued"), path.join("old")).unwrap();
    std::os::unix::fs::symlink(path.join("outside"), path.join("queued")).unwrap();
    cancel.cancel();
    assert!(matches!(
        collect(&scanner).last(),
        Some(ScanEventKind::Failed(_))
    ));
}

#[test]
fn validation_only_does_not_consume_entries() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    std::fs::write(path.join("file"), "").unwrap();
    let scanner = Scanner::new(ExplorationPolicy { home: None });
    let mut req = request(&path, 0);
    req.list_children = false;
    scanner.try_submit(req, true).unwrap();
    assert!(matches!(
        collect(&scanner).as_slice(),
        [ScanEventKind::Done]
    ));
}
