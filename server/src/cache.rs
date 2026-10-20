//! Where previews live, and how they are written.
//!
//! A preview for `/work/flow.dot` is written to
//! `<cache>/zed-graphviz/<hash>/flow.png`, where `<hash>` is a short hash of
//! the absolute source path. The file name keeps the source stem so the Zed
//! tab is titled `flow.png`, and the hash keeps two `flow.dot` files in
//! different folders apart.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Overrides the platform cache directory (used by the tests).
pub const CACHE_DIR_ENV: &str = "GRAPHVIZ_PREVIEW_CACHE_DIR";

/// `<platform cache dir>/zed-graphviz`, or `$GRAPHVIZ_PREVIEW_CACHE_DIR/zed-graphviz`.
pub fn cache_root() -> Option<PathBuf> {
    let base = match std::env::var_os(CACHE_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::cache_dir()?,
    };
    Some(base.join("zed-graphviz"))
}

/// First 16 hex digits of the SHA-256 of `input`.
pub fn short_hash(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The file stem used for previews and exports; `graph` when there is none.
pub fn stem(source: &Path) -> String {
    source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "graph".to_string())
}

/// Preview path for a document identified by `key` (its absolute path, or
/// its URI when it has none) with the given stem.
pub fn preview_path(root: &Path, key: &str, stem: &str) -> PathBuf {
    root.join(short_hash(key.as_bytes()))
        .join(format!("{stem}.png"))
}

/// Writes `bytes` to `path` through a temporary file in the same directory
/// and a rename, so a viewer never reads a half-written image.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        drop(file);
        match fs::rename(&tmp, path) {
            // Windows refuses to replace a file another program holds open
            // without delete sharing; overwrite it in place instead.
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
                fs::copy(&tmp, path).map(|_| ())
            }
            other => other,
        }
    })();
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_path_keeps_the_stem_and_separates_folders() {
        let root = Path::new("/cache/zed-graphviz");
        let a = preview_path(root, "/work/a/flow.dot", "flow");
        let b = preview_path(root, "/work/b/flow.dot", "flow");
        assert_eq!(a.file_name().unwrap(), "flow.png");
        assert_eq!(b.file_name().unwrap(), "flow.png");
        assert_ne!(a.parent(), b.parent());
        assert_eq!(a.parent().unwrap().parent().unwrap(), root);
        // Stable across runs: the same source always maps to the same file,
        // which is what lets Zed reuse the open tab.
        assert_eq!(a, preview_path(root, "/work/a/flow.dot", "flow"));
        let hash = a.parent().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stems() {
        assert_eq!(stem(Path::new("/x/flow.dot")), "flow");
        assert_eq!(stem(Path::new("/x/my.graph.gv")), "my.graph");
        assert_eq!(stem(Path::new("/")), "graph");
    }

    #[test]
    fn short_hash_is_sha256_prefix() {
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(short_hash(b"abc"), "ba7816bf8f01cfea");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("g.png");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}
