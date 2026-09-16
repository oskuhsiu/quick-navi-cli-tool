use qnav::model::*;
use std::path::{Path, PathBuf};

#[test]
fn node_keys_preserve_root_occurrences_and_depth() {
    let a = NodeKey::root(1)
        .child("repo".as_ref())
        .child("docs".as_ref());
    let b = NodeKey::root(2).child("docs".as_ref());
    assert_ne!(a, b);
    assert_eq!(a.depth(), 2);
    assert_eq!(a.chain().len(), 3);
    assert_eq!(b.parent(), Some(NodeKey::root(2)));
}

#[test]
fn ids_never_reuse_and_cancellation_is_shared() {
    let mut ids = WorkIds::default();
    assert_eq!(ids.allocate(), Some(1));
    let c = CancelToken::default();
    let other = c.clone();
    c.cancel();
    assert!(other.is_cancelled());
    assert_eq!(ids.allocate(), Some(2));
}

#[test]
fn prune_before_discovery_but_explicit_roots_override() {
    let p = ExplorationPolicy {
        home: Some(PathBuf::from("/people/me")),
    };
    for name in [".cache", ".git", "build", "target", "node_modules"] {
        assert!(!p.allows(Path::new("/work"), &Path::new("/work").join(name)));
    }
    for name in [".codex", ".local", "Library"] {
        assert!(p.allows(Path::new("/work"), &Path::new("/work").join(name)));
    }
    assert!(!p.allows(Path::new("/people/me"), Path::new("/people/me/Library/a")));
    assert!(p.allows(
        Path::new("/people/me/Library"),
        Path::new("/people/me/Library/a")
    ));
    assert!(p.allows(Path::new("/work/build"), Path::new("/work/build")));
}

#[test]
fn native_path_display_cannot_inject_terminal_controls() {
    use std::os::unix::ffi::OsStrExt;
    let native = std::ffi::OsStr::from_bytes(b"repo\x1b[2J\xff\n");
    let display = display_os(native);
    assert!(!display.contains('\x1b'));
    assert!(!display.contains('\n'));
    assert!(display.contains("\\xff"));
    assert_eq!(display_path(Path::new("中文 空白")), "中文 空白");
}
