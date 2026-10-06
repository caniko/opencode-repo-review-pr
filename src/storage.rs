use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("state path cannot traverse symlinks");
        }
    }
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        bail!("state directory must be user-owned and mode 0700");
    }
    Ok(())
}

pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => bail!("state must be a regular file"),
        Ok(_) => Ok(Some(
            serde_json::from_slice(&read_private(path)?)
                .context("invalid durable state; refusing redispatch")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn open_private(path: &Path, flags: rustix::fs::OFlags) -> Result<File> {
    use std::os::unix::fs::MetadataExt;
    private_dir(path.parent().context("state file needs a private parent")?)?;
    let fd = rustix::fs::open(
        path,
        flags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_bits_truncate(0o600),
    )?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        bail!("state file must be private, user-owned, regular and unshared");
    }
    Ok(file)
}

fn read_private(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = open_private(path, rustix::fs::OFlags::RDONLY)?;
    let mut bytes = vec![];
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("durable state exceeded size bound");
    }
    Ok(bytes)
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path needs a parent")?;
    private_dir(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temporary = path.with_extension(format!("{}.{nonce}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Preserve the engine's create-new freshness signal while checking retained
/// anchors with the same private-file policy as consumer-owned locks.
pub fn validate_existing_lock(path: &Path) -> Result<()> {
    match open_private(path, rustix::fs::OFlags::RDWR) {
        Ok(_) => Ok(()),
        Err(error)
            if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn lock(path: &Path) -> Result<File> {
    private_dir(path.parent().context("lock needs a parent")?)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        bail!("lock must be a regular file");
    }
    let file = open_private(path, rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE)?;
    let started = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < std::time::Duration::from_secs(30) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => {
                return Err(e)
                    .context("request state lock busy; resume without independent dispatch");
            }
        }
    }
    Ok(file)
}
