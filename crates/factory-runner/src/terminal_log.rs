use std::{io, os::unix::fs::FileExt as _, path::PathBuf, sync::Arc};

use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    net::unix::OwnedWriteHalf,
    sync::{Mutex, broadcast},
};

use super::{Error, TERMINAL_READ_CHUNK, send_terminal_output};

const TERMINAL_BROADCAST_CAPACITY: usize = 64;
pub(super) const TERMINAL_LOG_FILE: &str = "terminal.log";
const TERMINAL_LOG_ROTATED_FILE: &str = "terminal.log.1";

/// Retained, bounded, raw byte log for one terminal-mode run's PTY output.
///
/// Unlike [`EventLog`], this is not part of the durable command-acknowledgement
/// path: bytes are appended best-effort (no `sync_data`) purely so an operator
/// can inspect or re-attach to a run's terminal after the fact. Positions in
/// the log are a single monotonic byte-stream offset, independent of which
/// physical file currently holds a given byte.
pub(super) struct TerminalLog {
    dir: PathBuf,
    max_bytes: u64,
    inner: Mutex<TerminalLogInner>,
    chunks: broadcast::Sender<TerminalChunk>,
}

struct TerminalLogInner {
    active_file: File,
    generation: u64,
    active_start_offset: u64,
    active_len: u64,
    /// Start offset and length of `terminal.log.1`, the previous rotation,
    /// when one exists.
    previous: Option<RetainedSegment>,
}

struct RetainedSegment {
    generation: u64,
    start_offset: u64,
    len: u64,
    file: File,
}

impl TerminalLogInner {
    const fn total_bytes(&self) -> u64 {
        self.active_start_offset + self.active_len
    }

    const fn oldest_retained_offset(&self) -> u64 {
        match self.previous {
            Some(ref segment) => segment.start_offset,
            None => self.active_start_offset,
        }
    }
}

#[derive(Clone)]
pub(super) struct TerminalChunk {
    pub(super) generation: u64,
    pub(super) offset: u64,
    pub(super) bytes: Arc<Vec<u8>>,
}

pub(super) struct SnapshotSegment {
    pub(super) generation: u64,
    pub(super) start_offset: u64,
    pub(super) len: u64,
    file: std::fs::File,
}

pub(super) struct TerminalSnapshot {
    pub(super) generation: u64,
    pub(super) total_bytes: u64,
    pub(super) oldest_retained_offset: u64,
    pub(super) oldest_generation: u64,
    pub(super) active_start_offset: u64,
    active_file: std::fs::File,
    pub(super) previous: Option<SnapshotSegment>,
}

impl TerminalLog {
    pub(super) fn new(dir: PathBuf, max_bytes: u64, active_file: File) -> Arc<Self> {
        let (chunks, _) = broadcast::channel(TERMINAL_BROADCAST_CAPACITY);
        Arc::new(Self {
            dir,
            max_bytes: max_bytes.max(1),
            inner: Mutex::new(TerminalLogInner {
                active_file,
                generation: 0,
                active_start_offset: 0,
                active_len: 0,
                previous: None,
            }),
            chunks,
        })
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<TerminalChunk> {
        self.chunks.subscribe()
    }

    pub(super) async fn snapshot(&self) -> Result<TerminalSnapshot, Error> {
        let inner = self.inner.lock().await;
        let active_file = inner.active_file.try_clone().await?.into_std().await;
        let previous = match inner.previous.as_ref() {
            Some(segment) => Some(SnapshotSegment {
                generation: segment.generation,
                start_offset: segment.start_offset,
                len: segment.len,
                file: segment.file.try_clone().await?.into_std().await,
            }),
            None => None,
        };
        Ok(TerminalSnapshot {
            generation: inner.generation,
            total_bytes: inner.total_bytes(),
            oldest_retained_offset: inner.oldest_retained_offset(),
            oldest_generation: inner
                .previous
                .as_ref()
                .map_or(inner.generation, |segment| segment.generation),
            active_start_offset: inner.active_start_offset,
            active_file,
            previous,
        })
    }

    /// Finds a replay boundary that cannot begin inside a UTF-8 code point or
    /// an ANSI control sequence. The reset prefix sent with Ready establishes
    /// a documented baseline before this bounded suffix is applied; it is not
    /// an application-state checkpoint.
    pub(super) async fn safe_tail_start(
        &self,
        snapshot: &TerminalSnapshot,
        from: u64,
        through: u64,
    ) -> Result<u64, Error> {
        const INSPECTION_BYTES: u64 = 4096;
        let bytes = self
            .read_snapshot_bytes(snapshot, from, through.min(from + INSPECTION_BYTES))
            .await?;
        Ok(from + u64::try_from(safe_terminal_prefix(&bytes)).expect("prefix fits u64"))
    }

    async fn read_snapshot_bytes(
        &self,
        snapshot: &TerminalSnapshot,
        from: u64,
        through: u64,
    ) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        if let Some(segment) = snapshot.previous.as_ref() {
            read_file_range(
                &segment.file,
                segment.start_offset,
                segment.len,
                from,
                through,
                &mut bytes,
            )
            .await?;
        }
        let active_len = snapshot.total_bytes - snapshot.active_start_offset;
        read_file_range(
            &snapshot.active_file,
            snapshot.active_start_offset,
            active_len,
            from,
            through,
            &mut bytes,
        )
        .await?;
        Ok(bytes)
    }

    /// Appends raw bytes, rotating the active file when it is full, then
    /// broadcasts the whole chunk (as one unit, regardless of whether it was
    /// physically split across a rotation) to live subscribers.
    pub(super) async fn append(&self, bytes: Vec<u8>) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().await;
        let start_offset = inner.total_bytes();
        let mut remaining: &[u8] = &bytes;
        let mut published = Vec::new();
        while !remaining.is_empty() {
            let space = self.max_bytes.saturating_sub(inner.active_len);
            if space == 0 {
                self.rotate(&mut inner).await?;
                continue;
            }
            let take = remaining
                .len()
                .min(usize::try_from(space).unwrap_or(usize::MAX));
            inner.active_file.write_all(&remaining[..take]).await?;
            inner.active_len += take as u64;
            published.push(TerminalChunk {
                generation: inner.generation,
                offset: start_offset + (bytes.len() - remaining.len()) as u64,
                bytes: Arc::new(remaining[..take].to_vec()),
            });
            remaining = &remaining[take..];
        }
        inner.active_file.flush().await?;
        drop(inner);
        for chunk in published {
            let _ = self.chunks.send(chunk);
        }
        Ok(())
    }

    async fn rotate(&self, inner: &mut TerminalLogInner) -> Result<(), Error> {
        inner.active_file.flush().await?;
        let active_path = self.dir.join(TERMINAL_LOG_FILE);
        let rotated_path = self.dir.join(TERMINAL_LOG_ROTATED_FILE);
        tokio::fs::rename(&active_path, &rotated_path).await?;
        let fresh = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .mode(0o600)
            .open(&active_path)
            .await?;
        let previous_file = std::mem::replace(&mut inner.active_file, fresh);
        inner.previous = Some(RetainedSegment {
            generation: inner.generation,
            start_offset: inner.active_start_offset,
            len: inner.active_len,
            file: previous_file,
        });
        inner.generation += 1;
        inner.active_start_offset += inner.active_len;
        inner.active_len = 0;
        Ok(())
    }

    /// Replays retained bytes `[from, through)` to `write` as `TerminalOutput`
    /// frames, reading `terminal.log.1` (if it covers any of the range) then
    /// `terminal.log`, using the file boundaries already fixed in `snapshot`.
    ///
    /// The snapshot owns independent file descriptions and every read is
    /// positional, so concurrent appends cannot move a shared cursor or alter
    /// which inode the replay observes.
    pub(super) async fn replay(
        &self,
        write: &mut OwnedWriteHalf,
        snapshot: &TerminalSnapshot,
        from: u64,
        through: u64,
    ) -> Result<(), Error> {
        let mut cursor = from;
        if let Some(segment) = snapshot.previous.as_ref() {
            cursor = self
                .replay_file(
                    write,
                    &segment.file,
                    segment.generation,
                    segment.start_offset,
                    segment.len,
                    cursor,
                    through,
                )
                .await?;
        }
        let active_len = snapshot.total_bytes - snapshot.active_start_offset;
        self.replay_file(
            write,
            &snapshot.active_file,
            snapshot.generation,
            snapshot.active_start_offset,
            active_len,
            cursor,
            through,
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn replay_file(
        &self,
        write: &mut OwnedWriteHalf,
        file: &std::fs::File,
        generation: u64,
        file_start: u64,
        file_len: u64,
        cursor: u64,
        through: u64,
    ) -> Result<u64, Error> {
        let file_end = file_start + file_len;
        let read_from = cursor.max(file_start);
        let read_through = through.min(file_end);
        if read_from >= read_through {
            return Ok(cursor);
        }
        let mut remaining = read_through - read_from;
        let mut position = read_from;
        let mut buffer = vec![0_u8; TERMINAL_READ_CHUNK];
        while remaining > 0 {
            let want = usize::try_from(remaining.min(TERMINAL_READ_CHUNK as u64))
                .expect("chunk size fits usize");
            let read = file.read_at(&mut buffer[..want], position - file_start)?;
            if read != want {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal snapshot ended during positional replay",
                )));
            }
            send_terminal_output(write, generation, position, &buffer[..want]).await?;
            position += want as u64;
            remaining -= want as u64;
        }
        Ok(position.max(cursor))
    }
}

async fn read_file_range(
    source: &std::fs::File,
    file_start: u64,
    file_len: u64,
    from: u64,
    through: u64,
    output: &mut Vec<u8>,
) -> Result<(), Error> {
    let start = from.max(file_start);
    let end = through.min(file_start + file_len);
    if start >= end {
        return Ok(());
    }
    let mut remaining = usize::try_from(end - start).expect("bounded terminal range fits usize");
    let mut buffer = [0_u8; 4096];
    let mut position = start;
    while remaining > 0 {
        let take = remaining.min(buffer.len());
        let read = source.read_at(&mut buffer[..take], position - file_start)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        remaining -= read;
        position += read as u64;
    }
    Ok(())
}

pub(super) fn safe_terminal_prefix(bytes: &[u8]) -> usize {
    let mut index = 0;
    while index < bytes.len() && (bytes[index] & 0xc0) == 0x80 {
        index += 1;
    }
    while index < bytes.len() {
        match std::str::from_utf8(&bytes[index..]) {
            Ok(_) => break,
            Err(error) if error.valid_up_to() > 0 => {
                index += error.valid_up_to();
                break;
            }
            Err(error) if error.error_len().is_none() => {
                index += 1;
            }
            Err(_) => {
                index += 1;
            }
        }
    }
    if bytes.get(index) == Some(&0x1b) {
        return ansi_sequence_end(bytes, index + 1);
    }
    // A tail can begin after the ESC byte of a CSI/OSC sequence. Treat the
    // parameter introducer as part of that incomplete sequence as well.
    if matches!(bytes.get(index), Some(b'[' | b']' | b'P' | b'^' | b'_')) {
        return ansi_sequence_end(bytes, index + 1);
    }
    index
}

pub(super) fn generation_at(snapshot: &TerminalSnapshot, offset: u64) -> u64 {
    snapshot
        .previous
        .as_ref()
        .map_or(snapshot.generation, |segment| {
            if offset >= segment.start_offset && offset <= segment.start_offset + segment.len {
                segment.generation
            } else {
                snapshot.generation
            }
        })
}

fn ansi_sequence_end(bytes: &[u8], mut index: usize) -> usize {
    if matches!(bytes.get(index), Some(b'[' | b']' | b'P' | b'^' | b'_')) {
        index += 1;
    }
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    index
}

#[cfg(test)]
pub(super) fn open_terminal_log(
    dir: &std::path::Path,
    max_bytes: u64,
) -> std::sync::Arc<TerminalLog> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .mode(0o600)
        .open(dir.join(TERMINAL_LOG_FILE))
        .unwrap();
    TerminalLog::new(
        dir.to_path_buf(),
        max_bytes,
        tokio::fs::File::from_std(file),
    )
}

#[cfg(test)]
mod tests {
    use super::{TERMINAL_LOG_FILE, TERMINAL_LOG_ROTATED_FILE, open_terminal_log};
    use factory_core::runner::{RunnerFrame, decode_terminal_bytes};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::UnixStream,
    };

    #[tokio::test]
    async fn terminal_log_rotates_exactly_once_dropping_the_oldest_generation() {
        let directory = tempfile::tempdir().unwrap();
        let log = open_terminal_log(directory.path(), 4);

        log.append(b"ABCD".to_vec()).await.unwrap(); // exactly fills the active file
        log.append(b"EFGH".to_vec()).await.unwrap(); // forces the first rotation
        log.append(b"IJKL".to_vec()).await.unwrap(); // forces a second rotation

        let snapshot = log.snapshot().await.unwrap();
        assert_eq!(snapshot.total_bytes, 12);
        assert_eq!(
            snapshot.oldest_retained_offset, 4,
            "the first generation was dropped"
        );
        assert_eq!(snapshot.active_start_offset, 8);
        let previous = snapshot.previous.as_ref().unwrap();
        assert_eq!(previous.generation, 1);
        assert_eq!(previous.start_offset, 4);
        assert_eq!(previous.len, 4);
        assert_eq!(
            std::fs::read(directory.path().join(TERMINAL_LOG_ROTATED_FILE)).unwrap(),
            b"EFGH"
        );
        assert_eq!(
            std::fs::read(directory.path().join(TERMINAL_LOG_FILE)).unwrap(),
            b"IJKL"
        );
    }

    #[tokio::test]
    async fn terminal_log_replay_stitches_the_rotated_and_active_files() {
        let directory = tempfile::tempdir().unwrap();
        let log = open_terminal_log(directory.path(), 4);
        log.append(b"ABCD".to_vec()).await.unwrap();
        log.append(b"EFGH".to_vec()).await.unwrap();
        log.append(b"IJKL".to_vec()).await.unwrap();
        let snapshot = log.snapshot().await.unwrap();
        let total_bytes = snapshot.total_bytes;

        let (mut client, server) = UnixStream::pair().unwrap();
        let (_read, mut write) = server.into_split();
        log.replay(&mut write, &snapshot, 4, total_bytes)
            .await
            .unwrap();
        drop(write);

        let mut reader = BufReader::new(&mut client);
        let mut collected = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            let frame: RunnerFrame = serde_json::from_str(line.trim_end()).unwrap();
            let RunnerFrame::TerminalOutput { bytes, .. } = frame else {
                panic!("expected terminal output, got {frame:?}");
            };
            collected.extend(decode_terminal_bytes(&bytes).unwrap());
            line.clear();
        }
        assert_eq!(collected, b"EFGHIJKL");
    }
}
