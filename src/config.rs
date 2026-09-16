//! Ordered, lossless root configuration with serialized atomic updates.
use crate::model::RootEntry;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    roots: Vec<StoredRoot>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRoot {
    id: u64,
    path: NativePath,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum NativePath {
    Text(String),
    Bytes { unix_bytes: Vec<u8> },
}

pub fn default_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|p| PathBuf::from(p).join(".config"))
        })
        .ok_or("HOME and XDG_CONFIG_HOME are unset")?;
    Ok(base.join("qnav/config.json"))
}

pub fn load(path: &Path) -> Result<Vec<RootEntry>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Cannot read configuration: {e}")),
    };
    let doc: Document =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid configuration: {e}"))?;
    if doc.version != 1 {
        return Err("Unsupported configuration version".into());
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    doc.roots
        .into_iter()
        .map(|root| {
            let bytes = match root.path {
                NativePath::Text(s) => s.into_bytes(),
                NativePath::Bytes { unix_bytes } => unix_bytes,
            };
            if bytes.contains(&0) {
                return Err("Root path contains NUL".into());
            }
            let path = PathBuf::from(OsString::from_vec(bytes));
            if !path.is_absolute()
                || path
                    .as_os_str()
                    .as_bytes()
                    .split(|b| *b == b'/')
                    .any(|part| part == b"." || part == b"..")
            {
                return Err("Root path must be a normalized absolute path".into());
            }
            if root.id == 0 || !ids.insert(root.id) || !paths.insert(path.clone()) {
                return Err("Duplicate root ID/path or zero root ID".into());
            }
            Ok(RootEntry { id: root.id, path })
        })
        .collect()
}

pub fn add_root(
    config_path: &Path,
    input: &str,
    start_cwd: &Path,
    home: Option<&Path>,
) -> Result<(Vec<RootEntry>, RootEntry), String> {
    if input.is_empty() || input.contains('\0') {
        return Err("Root path is empty or contains NUL".into());
    }
    let input_path = if let Some(rest) = input.strip_prefix("~/") {
        home.ok_or("HOME is unavailable")?.join(rest)
    } else {
        PathBuf::from(input)
    };
    let candidate = if input_path.is_absolute() {
        input_path
    } else {
        start_cwd.join(input_path)
    };
    // Path components remove trailing separators, so link/ cannot disguise a final symlink.
    let candidate: PathBuf = candidate.components().collect();
    let metadata =
        fs::symlink_metadata(&candidate).map_err(|e| format!("Cannot inspect root: {e}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Root must be a directory, not a symlink".into());
    }
    let path = fs::canonicalize(candidate).map_err(|e| format!("Cannot resolve root: {e}"))?;
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create configuration directory: {e}"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(parent.join("config.lock"))
        .map_err(|e| format!("Cannot open configuration lock: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // The stable lock file is never renamed; closing its descriptor releases the lock.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            return Err(format!("Cannot lock configuration: {error}"));
        }
        if Instant::now() >= deadline {
            return Err("Configuration is busy; try again".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut roots = load(config_path)?;
    if let Some(existing) = roots.iter().find(|r| r.path == path).cloned() {
        return Ok((roots, existing));
    }
    let id = roots
        .iter()
        .map(|r| r.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("Root IDs exhausted")?;
    let added = RootEntry { id, path };
    roots.push(added.clone());
    let document = Document {
        version: 1,
        roots: roots
            .iter()
            .map(|root| StoredRoot {
                id: root.id,
                path: match root.path.to_str() {
                    Some(s) => NativePath::Text(s.into()),
                    None => NativePath::Bytes {
                        unix_bytes: root.path.as_os_str().as_bytes().to_vec(),
                    },
                },
            })
            .collect(),
    };
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("Cannot create configuration temporary file: {e}"))?;
    serde_json::to_writer_pretty(&mut temp, &document)
        .map_err(|e| format!("Cannot encode configuration: {e}"))?;
    temp.write_all(b"\n")
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|e| format!("Cannot write configuration: {e}"))?;
    temp.persist(config_path)
        .map_err(|e| format!("Cannot replace configuration: {e}"))?;
    Ok((roots, added))
}
