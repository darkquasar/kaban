//! Page-backed SQLite export through an anonymous staging descriptor. No SQLite
//! pathname is ever opened by this VFS. The final copy also uses only descriptors.
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::WriteError;
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use sqlite_vfs::{DatabaseHandle, LockKind, OpenKind, OpenOptions, Vfs, WalDisabled};

const VFS_NAME: &str = "kaban-private-snapshot-staging-v1";
const DATABASE: &str = "snapshot";
type Pending = Arc<Mutex<Option<File>>>;
// sqlite-vfs 0.2.0's registration is permanent and its callback state is not
// synchronized. Register once, and serialize its entire connection lifetime.
static EXPORT: Mutex<()> = Mutex::new(());
static REGISTERED: OnceLock<Option<Pending>> = OnceLock::new();

fn unavailable() -> io::Error {
    io::Error::other("snapshot staging unavailable")
}

pub(super) fn persist(source: &Connection, output: BorrowedFd<'_>) -> Result<(), WriteError> {
    let staging = stage(source)?;
    let length = staging.metadata()?.len();
    let mut buffer = vec![0; 64 * 1024];
    let mut offset = 0;
    while offset < length {
        let count = usize::try_from((length - offset).min(buffer.len() as u64))
            .map_err(|_| unavailable())?;
        staging.read_exact_at(&mut buffer[..count], offset)?;
        let mut written = 0;
        while written < count {
            match rustix::io::pwrite(output, &buffer[written..count], offset + written as u64) {
                Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero).into()),
                Ok(count) => written += count,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(io::Error::from(error).into()),
            }
        }
        offset += count as u64;
    }
    rustix::fs::ftruncate(output, length).map_err(io::Error::from)?;
    Ok(())
}

fn stage(source: &Connection) -> Result<File, WriteError> {
    let _exclusive = EXPORT.lock().map_err(|_| unavailable())?;
    let pending = REGISTERED
        .get_or_init(|| {
            let pending = Pending::default();
            sqlite_vfs::register(VFS_NAME, DescriptorVfs(pending.clone()), false)
                .ok()
                .map(|()| pending)
        })
        .as_ref()
        .ok_or_else(unavailable)?;
    // tempfile creates an anonymous 0600 file (unlinked on Darwin). We retain
    // its descriptor; SQLite never receives or resolves a filesystem path.
    let staging = tempfile::tempfile()?;
    // tempfile's Unix fallback ignores unlink failures. Refuse before writing
    // private graph bytes unless the file really has no remaining name.
    if staging.metadata()?.nlink() != 0 {
        return Err(unavailable().into());
    }
    *pending.lock().map_err(|_| unavailable())? = Some(staging.try_clone()?);
    let result = (|| {
        let mut destination = Connection::open_with_flags_and_vfs(
            DATABASE,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            VFS_NAME,
        )?;
        destination.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-1024; PRAGMA temp_store=MEMORY;")?;
        {
            let backup = Backup::new(source, &mut destination)?;
            loop {
                match backup.step(64)? {
                    StepResult::Done => break,
                    StepResult::More => {}
                    // Private finished source and destination cannot contend.
                    // Unexpected contention fails closed rather than waiting forever.
                    _ => return Err(io::Error::other("snapshot backup did not progress").into()),
                }
            }
        }
        destination
            .close()
            .map_err(|(_, error)| WriteError::from(error))
    })();
    // Also release the descriptor if opening failed before Vfs::open took it.
    pending.lock().map_err(|_| unavailable())?.take();
    result?;
    Ok(staging)
}

struct DescriptorVfs(Pending);

impl Vfs for DescriptorVfs {
    type Handle = Descriptor;
    fn open(&self, name: &str, options: OpenOptions) -> io::Result<Descriptor> {
        if name != DATABASE || options.kind != OpenKind::MainDb {
            return Err(io::Error::from(ErrorKind::PermissionDenied));
        }
        let file = self
            .0
            .lock()
            .map_err(|_| unavailable())?
            .take()
            .ok_or_else(unavailable)?;
        Ok(Descriptor {
            file,
            lock: LockKind::None,
        })
    }
    fn delete(&self, _: &str) -> io::Result<()> {
        Err(io::Error::from(ErrorKind::PermissionDenied))
    }
    fn exists(&self, name: &str) -> io::Result<bool> {
        Ok(name == DATABASE)
    }
    fn temporary_name(&self) -> String {
        // All secondary files are refused; no filesystem names are generated.
        String::new()
    }
    fn random(&self, buffer: &mut [i8]) {
        let mut random = [0_u8; 64];
        for chunk in buffer.chunks_mut(random.len()) {
            // SQLite randomness is not used for access control or staging names.
            // The trait cannot report entropy failure; zero is the fallback.
            if getrandom::fill(&mut random).is_err() {
                random.fill(0);
            }
            for (output, byte) in chunk.iter_mut().zip(random) {
                *output = i8::from_ne_bytes([byte]);
            }
        }
    }
    fn sleep(&self, duration: Duration) -> Duration {
        std::thread::sleep(duration);
        duration
    }
}

struct Descriptor {
    file: File,
    lock: LockKind,
}

impl DatabaseHandle for Descriptor {
    type WalIndex = WalDisabled;
    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn read_exact_at(&mut self, buffer: &mut [u8], offset: u64) -> io::Result<()> {
        // SQLite requires zero padding on SHORT_READ, including an empty header.
        buffer.fill(0);
        self.file.read_exact_at(buffer, offset)
    }
    fn write_all_at(&mut self, buffer: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buffer, offset)
    }
    fn sync(&mut self, data_only: bool) -> io::Result<()> {
        if data_only {
            self.file.sync_data()
        } else {
            self.file.sync_all()
        }
    }
    fn set_len(&mut self, length: u64) -> io::Result<()> {
        self.file.set_len(length)
    }
    fn lock(&mut self, lock: LockKind) -> io::Result<bool> {
        // Exactly one SQLite handle can consume the pending anonymous file.
        self.lock = lock;
        Ok(true)
    }
    fn reserved(&mut self) -> io::Result<bool> {
        Ok(self.lock >= LockKind::Reserved)
    }
    fn current_lock(&self) -> io::Result<LockKind> {
        Ok(self.lock)
    }
    fn wal_index(&self, _: bool) -> io::Result<WalDisabled> {
        Err(io::Error::from(ErrorKind::PermissionDenied))
    }
}
