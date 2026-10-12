//! Reading and writing files that hold a private key or a password, and the folder they live in.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::{Context, Result, err};

/// Whether a file with this mode is closed to other users: readable by its owner alone.
///
/// Not even its group may read it. The application joins swisha's group to reach the socket, so
/// a group read would hand it the database password, the API secret and every private key.
pub fn is_private(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Reads a whole file, refusing one other users can open. `what` names the file in a read error.
/// The mode is read from the opened file itself, so the file checked is the file read.
pub fn read_private(path: &Path, what: &str) -> Result<String> {
    let cannot_read = || format!("Cannot read {what}: {}", path.display());
    let mut file = std::fs::File::open(path).with_context(cannot_read)?;
    let meta = file.metadata().with_context(cannot_read)?;
    if !is_private(meta.mode()) {
        return Err(err(format!(
            "{} is open to other users (mode {:04o}), and it holds a key or a password. Make it \
             owned by the user swisha runs as, and readable by it alone: chmod 600 {}",
            path.display(),
            meta.mode() & 0o7777,
            path.display(),
        )));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).with_context(cannot_read)?;
    Ok(contents)
}

/// Writes a new file readable by its owner alone, and flushes it to disk. Never replaces a file
/// that already exists.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let cannot_write = || format!("Cannot write {}", path.display());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(cannot_write)?;
    file.write_all(contents.as_bytes()).with_context(cannot_write)?;
    file.sync_all().with_context(cannot_write)
}

/// Creates a directory readable by its owner alone.
pub fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("Cannot create {}", path.display()))
}

/// Flushes a directory's entries to disk, so a rename inside it survives a crash.
pub fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("Cannot flush {}", path.display()))
}

/// Checks a directory swisha keeps private keys in: it exists, other users cannot open it, by the
/// rule [`is_private`] applies to a file, and swisha can write to it. `what` names it in an error.
pub fn check_private_dir(path: &Path, what: &str) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("Cannot open {what}: {}", path.display()))?;
    if !meta.is_dir() {
        return Err(err(format!("{what} {} is not a directory", path.display())));
    }
    if !is_private(meta.mode()) {
        return Err(err(format!(
            "{} is open to other users (mode {:04o}), and it holds private keys. Make it owned by \
             the user swisha runs as, and open to it alone: chmod 700 {}",
            path.display(),
            meta.mode() & 0o7777,
            path.display(),
        )));
    }
    let probe = path.join(format!(".probe-{}", crate::swish::random_payout_uuid()));
    write_private(&probe, "").with_context(|| format!("swisha cannot write to {what} {}", path.display()))?;
    std::fs::remove_file(&probe).with_context(|| format!("Cannot remove {}", probe.display()))
}
