use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Explicit roots must already exist. Relative explicit roots are resolved once;
    /// the default never depends on the process working directory.
    pub fn new(explicit: Option<PathBuf>) -> Result<Self> {
        let root = match explicit {
            Some(root) => {
                ensure!(!root.as_os_str().is_empty(), "application root is empty");
                ensure!(
                    !root
                        .components()
                        .any(|part| matches!(part, Component::ParentDir)),
                    "application root must not contain parent traversal"
                );
                if root.is_absolute() {
                    root
                } else {
                    std::env::current_dir()
                        .context("resolve explicit application root")?
                        .join(root)
                }
            }
            None => {
                let executable = std::env::current_exe()
                    .context("locate executable")?
                    .canonicalize()
                    .context("resolve real executable")?;
                root_from_executable(&executable)?
            }
        };
        check_components(&root)?;
        ensure!(
            root.is_dir(),
            "application root must be an existing directory"
        );
        let root = root.canonicalize().context("resolve application root")?;
        Ok(Self { root })
    }

    pub fn prepare(&self) -> Result<()> {
        self.validate_root()?;
        private_directory(&self.root)?;
        for name in ["config", "data", "logs", "tmp", "run", "backups"] {
            let directory = self.file(name)?;
            if !directory.exists() {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error).context("create application directory"),
                }
            }
            check_components(&directory)?;
            private_directory(&directory)?;
        }
        Ok(())
    }

    pub fn file(&self, relative: &str) -> Result<PathBuf> {
        self.validate_root()?;
        // Use one portable syntax. In particular ':' would permit Windows ADS,
        // and backslashes must not silently become separators on another OS.
        ensure!(
            !relative.is_empty() && !relative.contains(['\\', ':', '\0']),
            "invalid application-relative path"
        );
        for part in relative.split('/') {
            ensure!(
                !part.is_empty() && part != "." && part != "..",
                "path must contain only normal relative components"
            );
            ensure!(
                !part.ends_with(['.', ' ']),
                "ambiguous application-relative path"
            );
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            ensure!(
                !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    && !(stem.len() == 4
                        && (stem.starts_with("COM") || stem.starts_with("LPT"))
                        && stem.as_bytes()[3].is_ascii_digit()),
                "reserved application-relative path"
            );
        }
        ensure!(
            Path::new(relative)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
            "path must be relative to the application root"
        );
        let path = self.root.join(relative);
        check_components(&path)?;
        if path.exists() {
            ensure!(
                path.canonicalize()
                    .context("resolve application path")?
                    .starts_with(&self.root),
                "application path escapes root"
            );
        }
        Ok(path)
    }

    pub fn lock(&self) -> Result<InstanceLock> {
        let file = self.open_lock()?;
        file.try_lock_exclusive()
            .context("another instance holds the application lock")?;
        // Never unlink the lock file: another process may already hold its inode.
        Ok(InstanceLock { file })
    }

    /// A snapshot, not a reservation. Only lock contention means running;
    /// filesystem and permission errors remain errors.
    pub fn is_locked(&self) -> Result<bool> {
        let file = self.open_lock()?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&file).context("release instance status lock")?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            // fs2 returns ERROR_LOCK_VIOLATION on Windows, which Rust does not
            // classify as WouldBlock. Normalize only that precise contention
            // code; permission failures must never masquerade as a live app.
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(true)
            }
            Err(error) => Err(error).context("probe instance lock"),
        }
    }

    fn open_lock(&self) -> Result<File> {
        let path = self.file("run/instance.lock")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        private_file_options(&mut options);
        let file = options.open(&path).context("open instance lock")?;
        ensure!(
            file.metadata()?.is_file(),
            "instance lock is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .context("protect instance lock")?;
        }
        Ok(file)
    }

    fn validate_root(&self) -> Result<()> {
        ensure!(self.root.is_absolute(), "application root must be absolute");
        check_components(&self.root)?;
        ensure!(self.root.is_dir(), "application root is not a directory");
        Ok(())
    }
}

pub struct InstanceLock {
    file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn root_from_executable(executable: &Path) -> Result<PathBuf> {
    let bin = executable
        .parent()
        .context("executable has no parent directory")?;
    ensure!(
        bin.file_name().is_some_and(|name| name == "bin"),
        "default root requires a real executable in <root>/bin; pass --root explicitly"
    );
    Ok(bin
        .parent()
        .context("executable has no application root")?
        .to_path_buf())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject junctions and other reparse points as well as ordinary symlinks.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Existing ancestors must be real directories, not links/reparse points. The
/// deployment root is private; these checks are not a sandbox against its owner
/// concurrently replacing directories during filesystem operations.
fn check_components(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "filesystem path must be absolute");
    let mut current = PathBuf::new();
    let parts: Vec<_> = path.components().collect();
    for (index, part) in parts.iter().enumerate() {
        ensure!(
            !matches!(part, Component::ParentDir),
            "parent traversal is forbidden"
        );
        current.push(part.as_os_str());
        // A Windows drive/UNC prefix is not yet a complete absolute path.
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    !is_link(&metadata),
                    "symbolic links and reparse points are forbidden"
                );
                if index + 1 < parts.len() {
                    ensure!(metadata.is_dir(), "path ancestor is not a directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect application path {}", current.display()));
            }
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "application directory is not a directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("protect application directory")?;
    }
    Ok(())
}

fn private_file_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// File contents are flushed before the single rename commit point. Rust's
/// Windows rename uses MoveFileExW with MOVEFILE_REPLACE_EXISTING, so it also
/// replaces existing regular files without deleting the old file first.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    check_components(path)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file(),
            "atomic write target is not a regular file"
        );
    }
    let parent = path.parent().context("atomic write target has no parent")?;
    ensure!(parent.is_dir(), "atomic write parent is not a directory");
    let mut created = None;
    for _ in 0..32 {
        let temporary = parent.join(format!(
            ".nestbot-write-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        private_file_options(&mut options);
        match options.open(&temporary) {
            Ok(file) => {
                created = Some((TemporaryFile(temporary), file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create atomic write temporary file"),
        }
    }
    let (temporary, mut file) = created.context("cannot allocate atomic write temporary file")?;
    file.write_all(bytes).context("write atomic replacement")?;
    file.sync_all().context("flush atomic replacement")?;
    drop(file);
    check_components(path)?;
    fs::rename(&temporary.0, path).context("commit atomic replacement")?;
    // After rename the update has committed. A directory-flush failure cannot
    // safely be reported as a rollback; attempt it without desynchronizing the
    // caller's in-memory state from the already committed file.
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn read_limited(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    check_components(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect input file"),
    };
    ensure!(metadata.is_file(), "input is not a regular file");
    ensure!(
        metadata.len() <= limit as u64,
        "input file exceeds size limit"
    );
    let file = File::open(path).context("open input file")?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read input file")?;
    if bytes.len() > limit {
        bail!("input file exceeds size limit");
    }
    Ok(Some(bytes))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub struct TestRoot(pub Paths);

    impl TestRoot {
        pub fn new(label: &str) -> Self {
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp/tests");
            fs::create_dir_all(&base).unwrap();
            let root = base.join(format!(
                "{label}-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let paths = Paths::new(Some(root)).unwrap();
            paths.prepare().unwrap();
            Self(paths)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestRoot;
    use super::*;

    #[test]
    fn default_root_requires_bin_layout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            root_from_executable(&root.join("bin/nestbot")).unwrap(),
            root
        );
        assert!(root_from_executable(&root.join("target/debug/nestbot")).is_err());
    }

    #[test]
    fn paths_reject_traversal_and_platform_escapes() {
        let root = TestRoot::new("paths");
        for bad in [
            "",
            "..",
            "../outside",
            "data/../outside",
            "/absolute",
            "C:/outside",
            "data\\outside",
            "data/x:stream",
            "data/.",
            "data//x",
            "data/NUL",
            "data/name.",
        ] {
            assert!(root.0.file(bad).is_err(), "accepted {bad}");
        }
        assert!(root.0.file("data/nested/file.toml").is_ok());
    }

    #[test]
    fn lock_is_exclusive_and_drop_releases_it() {
        let root = TestRoot::new("lock");
        assert!(!root.0.is_locked().unwrap());
        let lock = root.0.lock().unwrap();
        assert!(root.0.lock().is_err());
        assert!(root.0.is_locked().unwrap());
        drop(lock);
        assert!(!root.0.is_locked().unwrap());
        assert!(root.0.lock().is_ok());
    }

    #[test]
    fn lock_probe_propagates_invalid_filesystem_state() {
        let root = TestRoot::new("lock-error");
        let lock = root.0.file("run/instance.lock").unwrap();
        fs::create_dir(lock).unwrap();
        assert!(root.0.is_locked().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn failed_windows_replacement_preserves_original_and_cleans_temporary_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = TestRoot::new("atomic-sharing-failure");
        let path = root.0.file("data/state").unwrap();
        atomic_write(&path, b"original").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(atomic_write(&path, b"replacement").is_err());
        drop(held);
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert_eq!(
            fs::read_dir(root.0.file("data").unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn atomic_write_replaces_existing_file_and_cleans_temporary_files() {
        let root = TestRoot::new("atomic");
        let path = root.0.file("data/state").unwrap();
        atomic_write(&path, b"original").unwrap();
        atomic_write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"replacement");
        assert_eq!(
            fs::read_dir(root.0.file("data").unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn atomic_write_rejects_directory_without_touching_contents() {
        let root = TestRoot::new("atomic-failure");
        let path = root.0.file("data/state").unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("original"), b"keep").unwrap();
        assert!(atomic_write(&path, b"replacement").is_err());
        assert_eq!(fs::read(path.join("original")).unwrap(), b"keep");
    }

    #[test]
    fn bounded_reader_rejects_oversized_files() {
        let root = TestRoot::new("bounded-read");
        let path = root.0.file("data/input").unwrap();
        atomic_write(&path, b"12345").unwrap();
        assert!(read_limited(&path, 4).is_err());
        assert_eq!(read_limited(&path, 5).unwrap().unwrap(), b"12345");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_rejected_in_roots_parents_and_targets() {
        use std::os::unix::fs::symlink;
        let root = TestRoot::new("symlinks");
        let other = TestRoot::new("symlink-target");
        let link = root.0.root.join("data/linked");
        symlink(&other.0.root, &link).unwrap();
        assert!(root.0.file("data/linked/state").is_err());
        assert!(Paths::new(Some(link.clone())).is_err());
        assert!(atomic_write(&link.join("state"), b"bad").is_err());
        let dangling = root.0.root.join("data/dangling");
        symlink(other.0.root.join("missing"), &dangling).unwrap();
        assert!(root.0.file("data/dangling").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestRoot::new("permissions");
        let path = root.0.file("data/state").unwrap();
        atomic_write(&path, b"private").unwrap();
        let _lock = root.0.lock().unwrap();
        for name in ["config", "data", "logs", "tmp", "run", "backups"] {
            assert_eq!(
                fs::metadata(root.0.file(name).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        for name in ["data/state", "run/instance.lock"] {
            assert_eq!(
                fs::metadata(root.0.file(name).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
