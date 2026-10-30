//! Where Tantivy keeps its files: `MmapDirectory`, except that finished files are synced
//! with a plain `fsync`.
//!
//! On macOS, Rust's `File::sync_data` is `F_FULLFSYNC`, which also waits for the drive to
//! flush its cache: about 5ms each, and a commit syncs a dozen files, so adding two records
//! took 70-90ms. The index is a cache that can be rebuilt from the transcripts; surviving a
//! process or kernel crash is enough, a power cut is not worth that price.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    AntiCallToken, Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, TerminatingWrite,
    WatchCallback, WatchHandle, WritePtr,
};

#[derive(Clone, Debug)]
pub struct IndexDirectory {
    root: PathBuf,
    inner: MmapDirectory,
}

impl IndexDirectory {
    pub fn open(root: &Path) -> Result<IndexDirectory, OpenDirectoryError> {
        Ok(IndexDirectory {
            root: root.to_path_buf(),
            inner: MmapDirectory::open(root)?,
        })
    }
}

fn fsync(file: &File) -> io::Result<()> {
    // SAFETY: the descriptor belongs to `file`, which is open for the whole call.
    if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A file being written; synced once Tantivy is done with it.
struct SyncedFile(File);

impl Write for SyncedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl TerminatingWrite for SyncedFile {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.0.flush()?;
        fsync(&self.0)
    }
}

impl Directory for IndexDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.inner.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(path))
            .map_err(|err| {
                if err.kind() == io::ErrorKind::AlreadyExists {
                    OpenWriteError::FileAlreadyExists(path.to_path_buf())
                } else {
                    OpenWriteError::wrap_io_error(err, path.to_path_buf())
                }
            })?;
        Ok(BufWriter::new(Box::new(SyncedFile(file))))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    /// Write a temporary file next to the target, sync it, rename it over the target.
    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let target = self.root.join(path);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let temp = target.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            file.write_all(data)?;
            fsync(&file)?;
            std::fs::rename(&temp, &target)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(watch_callback)
    }

    fn sync_directory(&self) -> io::Result<()> {
        fsync(&File::open(&self.root)?)
    }
}
