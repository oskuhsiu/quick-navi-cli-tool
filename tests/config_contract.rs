use qnav::config::{add_root, load};
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::symlink, io::AsRawFd},
    path::Path,
    sync::{Arc, Barrier},
    time::Instant,
};

#[test]
fn add_preserves_literal_paths_and_deduplicates_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let config = base.join("settings/config.json");
    assert!(load(&config).unwrap().is_empty());
    assert!(!config.exists());
    fs::create_dir(base.join(" white\n")).unwrap();
    let (_, first) = add_root(&config, " white\n", &base, None).unwrap();
    assert_eq!(first.path, base.join(" white\n"));
    symlink(&base, base.join("alias")).unwrap();
    let (roots, duplicate) = add_root(&config, "alias/ white\n", &base, None).unwrap();
    assert_eq!(duplicate, first);
    assert_eq!(roots.len(), 1);
    symlink(&first.path, base.join("link")).unwrap();
    for input in ["link", "link/", "", "\0"] {
        assert!(add_root(&config, input, &base, None).is_err());
    }
    fs::create_dir(base.join("nested")).unwrap();
    assert_eq!(
        add_root(&config, "~/nested", Path::new("/"), Some(&base))
            .unwrap()
            .1
            .path,
        base.join("nested")
    );
    assert!(add_root(&config, "$HOME/nested", &base, Some(&base)).is_err());
}

#[test]
fn invalid_configuration_is_rejected_without_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    let invalid = [
        "broken",
        r#"{"version":2,"roots":[]}"#,
        r#"{"version":1,"roots":[{"id":0,"path":"/a"}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":"relative"}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":"/a"},{"id":1,"path":"/b"}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":"/a"},{"id":2,"path":{"unix_bytes":[47,97]}}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":{"unix_bytes":[47,0]}}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":"/a/../b"}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":"/a/./b"}]}"#,
        r#"{"version":1,"roots":[{"id":1,"path":{"unix_bytes":[47,97],"other":true}}]}"#,
    ];
    for original in invalid {
        fs::write(&config, original).unwrap();
        assert!(load(&config).is_err(), "{original}");
        assert!(add_root(&config, temp.path().to_str().unwrap(), Path::new("/"), None).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
    }
}

#[test]
fn native_bytes_and_missing_roots_survive_save() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    fs::write(
        &config,
        r#"{"version":1,"roots":[{"id":9,"path":{"unix_bytes":[47,255]}}]}"#,
    )
    .unwrap();
    let expected = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 255]));
    assert_eq!(load(&config).unwrap()[0].path, expected);
    let (roots, added) =
        add_root(&config, temp.path().to_str().unwrap(), Path::new("/"), None).unwrap();
    assert_eq!(added.id, 10);
    assert_eq!(load(&config).unwrap(), roots);
    assert!(fs::read_to_string(config).unwrap().contains("unix_bytes"));
}

#[test]
fn concurrent_additions_merge_and_lock_times_out() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let dir = temp.path().join(n.to_string());
            fs::create_dir(&dir).unwrap();
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                add_root(&config, dir.to_str().unwrap(), Path::new("/"), None).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(load(&config).unwrap().len(), 8);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join("config.lock"))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let start = Instant::now();
    let error = add_root(&config, temp.path().to_str().unwrap(), Path::new("/"), None).unwrap_err();
    assert!(error.contains("busy"));
    assert!(start.elapsed().as_secs_f64() >= 1.9 && start.elapsed().as_secs_f64() < 3.0);
}
