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

/// Who may read a written file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Only the user (previews in the cache).
    Private,
    /// Like a file the user saved (exports): a new file gets the usual
    /// creation mode (0666 less the umask) and a replaced file keeps its mode.
    Shared,
}

/// Writes `bytes` to `path` through a temporary file in the same directory
/// and a rename, so a viewer never reads a half-written image.
///
/// The temporary file has a random name and is created exclusively (and
/// readable only by the user on Unix), so a planted file or symlink can
/// never redirect the write. The rename replaces `path` itself, never what
/// a symlink at `path` points to.
pub fn write_atomic(path: &Path, bytes: &[u8], access: Access) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!(".{name}.");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp");
    // The mode passed at creation is reduced by the umask, like any new file.
    #[cfg(unix)]
    if access == Access::Shared {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut tmp = builder.tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    // Replacing a regular file keeps its permissions.
    #[cfg(unix)]
    if access == Access::Shared {
        if let Ok(existing) = fs::symlink_metadata(path) {
            if existing.is_file() {
                tmp.as_file().set_permissions(existing.permissions())?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = access;

    // Windows refuses to replace a file another program has open without
    // delete sharing; retry briefly, then give up rather than write in place.
    let mut attempts = 0;
    loop {
        match tmp.persist(path) {
            Ok(_) => return Ok(()),
            Err(e)
                if cfg!(windows)
                    && e.error.kind() == io::ErrorKind::PermissionDenied
                    && attempts < 5 =>
            {
                attempts += 1;
                tmp = e.file;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // Dropping the temporary file deletes it.
            Err(e) => return Err(e.error),
        }
    }
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
        write_atomic(&path, b"one", Access::Private).unwrap();
        write_atomic(&path, b"two", Access::Private).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn planted_symlinks_are_never_followed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, b"keep me").unwrap();
        let target = dir.path().join("out").join("g.png");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Symlinks at every name an earlier, predictable scheme would use.
        for n in 0..32 {
            let name = format!(".g.png.{}.{n}.tmp", std::process::id());
            symlink(&victim, target.parent().unwrap().join(name)).unwrap();
        }
        write_atomic(&target, b"preview", Access::Private).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert_eq!(fs::read(&target).unwrap(), b"preview");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A symlink at the destination is replaced, not written through.
        let link = dir.path().join("link.svg");
        symlink(&victim, &link).unwrap();
        write_atomic(&link, b"export", Access::Shared).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(!fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn exports_get_the_default_mode_or_keep_the_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        // What any program creating a file here gets (0666 less the umask).
        let reference = dir.path().join("reference");
        fs::File::create(&reference).unwrap();

        let new = dir.path().join("new.svg");
        write_atomic(&new, b"svg", Access::Shared).unwrap();
        assert_eq!(mode(&new), mode(&reference));

        let existing = dir.path().join("existing.png");
        fs::write(&existing, b"old").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&existing, b"new", Access::Shared).unwrap();
        assert_eq!(fs::read(&existing).unwrap(), b"new");
        assert_eq!(mode(&existing), 0o640);

        let preview = dir.path().join("preview.png");
        write_atomic(&preview, b"png", Access::Private).unwrap();
        assert_eq!(mode(&preview), 0o600);
    }
}
