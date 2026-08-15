//! Exclusive daemon ownership and local filesystem lifecycle.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

use rustix::fs::{FlockOperation, OFlags};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// The exclusive right to run one daemon for a database.
#[derive(Debug)]
pub struct DaemonInstance {
    database: PathBuf,
    socket: PathBuf,
    lock_path: PathBuf,
    _lock_file: File,
}

impl DaemonInstance {
    /// Secure the state paths and acquire the database's non-blocking lifetime lock.
    pub fn claim(database: &Path, socket: &Path) -> io::Result<Self> {
        ensure_private_parent(database)?;
        ensure_private_parent(socket)?;

        let database_file = open_private_regular_file(database, "database")?;
        let database = fs::canonicalize(database)?;
        drop(database_file);

        let lock_path = lock_path_for(&database);
        let lock_file = open_private_regular_file(&lock_path, "database lock")?;
        rustix::fs::flock(&lock_file, FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                let source = io::Error::from_raw_os_error(error.raw_os_error());
                io::Error::new(
                    source.kind(),
                    format!(
                        "cannot acquire database lock {}: {source}",
                        lock_path.display()
                    ),
                )
            },
        )?;

        Ok(Self {
            database,
            socket: canonical_child_path(socket, "socket")?,
            lock_path,
            _lock_file: lock_file,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Bind the configured socket, replacing only a confirmed stale socket inode.
    pub fn bind_socket(&self) -> io::Result<(UnixListener, SocketCleanup)> {
        recover_stale_socket(&self.socket)?;
        let listener = UnixListener::bind(&self.socket).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot bind local socket {}: {error}",
                    self.socket.display()
                ),
            )
        })?;
        let metadata = fs::symlink_metadata(&self.socket)?;
        if !metadata.file_type().is_socket() {
            return Err(invalid(format!(
                "bound socket {} is not a Unix socket",
                self.socket.display()
            )));
        }
        let identity = FileIdentity::from(&metadata);
        let cleanup = SocketCleanup {
            path: self.socket.clone(),
            identity,
            armed: true,
        };
        fs::set_permissions(&self.socket, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        let protected = fs::symlink_metadata(&self.socket)?;
        if !protected.file_type().is_socket()
            || FileIdentity::from(&protected) != identity
            || protected.mode() & 0o777 != PRIVATE_FILE_MODE
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "local socket {} is not private mode 0600",
                    self.socket.display()
                ),
            ));
        }
        Ok((listener, cleanup))
    }
}

/// Removes only the socket inode created by this daemon instance.
#[derive(Debug)]
pub struct SocketCleanup {
    path: PathBuf,
    identity: FileIdentity,
    armed: bool,
}

impl SocketCleanup {
    pub fn remove(mut self) -> io::Result<()> {
        self.armed = false;
        remove_if_same_socket(&self.path, self.identity)
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_if_same_socket(&self.path, self.identity);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl From<&fs::Metadata> for FileIdentity {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn ensure_private_parent(path: &Path) -> io::Result<()> {
    let parent = usable_parent(path)?;
    let existed = match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            verify_private_directory(parent, &metadata)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };

    if !existed {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(PRIVATE_DIRECTORY_MODE);
        builder.create(parent)?;
        let metadata = fs::symlink_metadata(parent)?;
        verify_private_directory(parent, &metadata)?;
    }
    Ok(())
}

fn verify_private_directory(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(invalid(format!(
            "state parent {} must not be a symbolic link",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(invalid(format!(
            "state parent {} is not a directory",
            path.display()
        )));
    }
    verify_owner(path, metadata)?;
    if metadata.mode() & 0o777 != PRIVATE_DIRECTORY_MODE {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "state parent {} must be owner-only mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn open_private_regular_file(path: &Path, label: &str) -> io::Result<File> {
    let existed = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(invalid(format!(
                    "{label} {} must be a regular file, not a symbolic link or special file",
                    path.display()
                )));
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };

    let nofollow = i32::try_from(OFlags::NOFOLLOW.bits())
        .map_err(|_| invalid("O_NOFOLLOW does not fit the platform open flags".into()))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(nofollow)
        .open(path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot open {label} {}: {error}", path.display()),
            )
        })?;
    if !existed {
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    }
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "{label} {} must be a regular file",
            path.display()
        )));
    }
    verify_owner(path, &metadata)?;
    if metadata.mode() & 0o777 != PRIVATE_FILE_MODE {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{label} {} must have mode 0600", path.display()),
        ));
    }
    Ok(file)
}

fn verify_owner(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let expected = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by the current user", path.display()),
        ));
    }
    Ok(())
}

fn recover_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-socket endpoint {}", path.display()),
        ));
    }

    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("local socket {} is already live", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) if error.kind() != io::ErrorKind::ConnectionRefused => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "cannot confirm whether socket {} is stale: {error}",
                    path.display()
                ),
            ));
        }
        Err(_) => {}
    }

    let expected = FileIdentity::from(&metadata);
    let current = fs::symlink_metadata(path)?;
    if !current.file_type().is_socket() || FileIdentity::from(&current) != expected {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("socket endpoint {} changed during recovery", path.display()),
        ));
    }
    fs::remove_file(path)
}

fn remove_if_same_socket(path: &Path, expected: FileIdentity) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_socket() && FileIdentity::from(&metadata) == expected {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn lock_path_for(database: &Path) -> PathBuf {
    let mut path = OsString::from(database.as_os_str());
    path.push(".lock");
    PathBuf::from(path)
}

fn canonical_child_path(path: &Path, label: &str) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid(format!("{label} path {} has no file name", path.display())))?;
    Ok(fs::canonicalize(usable_parent(path)?)?.join(name))
}

fn usable_parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            invalid(format!(
                "{} needs a dedicated parent directory",
                path.display()
            ))
        })
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Wait for either interactive interrupt or the service-manager termination signal.
pub async fn shutdown_signal() -> io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}
