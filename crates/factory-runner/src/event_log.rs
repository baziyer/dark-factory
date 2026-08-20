use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use factory_core::runner::{
    MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_OUTPUT_TEXT_BYTES,
    MAX_RUNNER_SPOOL_BYTES, OutputStream, RUNNER_PROTOCOL_VERSION, RunnerEvent,
    RunnerEventEnvelope,
};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    sync::{Mutex, broadcast},
};

use super::Error;

const BROADCAST_CAPACITY: usize = 32;
const TERMINAL_RESERVE_BYTES: usize = MAX_RUNNER_ERROR_BYTES + 4096;

pub(super) struct EventLog {
    spool_path: PathBuf,
    inner: Mutex<LogInner>,
    events: broadcast::Sender<RunnerEventEnvelope>,
}

struct LogInner {
    file: BufWriter<File>,
    head: i64,
    terminal_sequence: Option<i64>,
    bytes: usize,
    output_truncated: bool,
}

#[derive(Clone, Copy)]
pub(super) struct LogSnapshot {
    pub(super) head: i64,
    pub(super) terminal_sequence: Option<i64>,
}

impl EventLog {
    pub(super) fn new(spool_path: PathBuf, file: File) -> Arc<Self> {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            spool_path,
            inner: Mutex::new(LogInner {
                file: BufWriter::new(file),
                head: 0,
                terminal_sequence: None,
                bytes: 0,
                output_truncated: false,
            }),
            events,
        })
    }

    pub(super) fn spool_path(&self) -> &Path {
        &self.spool_path
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<RunnerEventEnvelope> {
        self.events.subscribe()
    }

    pub(super) async fn snapshot(&self) -> LogSnapshot {
        let inner = self.inner.lock().await;
        LogSnapshot {
            head: inner.head,
            terminal_sequence: inner.terminal_sequence,
        }
    }

    pub(super) async fn append_output(
        &self,
        stream: OutputStream,
        text: String,
        lossy: bool,
    ) -> Result<(), Error> {
        debug_assert!(text.len() <= MAX_RUNNER_OUTPUT_TEXT_BYTES);
        let event = RunnerEvent::Output {
            stream,
            text,
            lossy,
        };
        let published = {
            let mut inner = self.inner.lock().await;
            if inner.terminal_sequence.is_some() || inner.output_truncated {
                return Ok(());
            }
            let envelope = next_envelope(&inner, event);
            let encoded = encode_event(&envelope)?;
            if inner.bytes + encoded.len() + TERMINAL_RESERVE_BYTES > MAX_RUNNER_SPOOL_BYTES {
                inner.output_truncated = true;
                let truncated = next_envelope(
                    &inner,
                    RunnerEvent::OutputTruncated {
                        limit_bytes: u64::try_from(MAX_RUNNER_SPOOL_BYTES)
                            .expect("spool limit fits u64"),
                    },
                );
                Some(append_encoded(&mut inner, truncated, false).await?)
            } else {
                Some(append_encoded(&mut inner, envelope, false).await?)
            }
        };
        if let Some(event) = published {
            let _ = self.events.send(event);
        }
        Ok(())
    }

    pub(super) async fn append_lifecycle(
        &self,
        event: RunnerEvent,
        terminal: bool,
    ) -> Result<i64, Error> {
        let published = {
            let mut inner = self.inner.lock().await;
            if inner.terminal_sequence.is_some() {
                return Err(Error::Task(
                    "attempted to append a second terminal event".into(),
                ));
            }
            let envelope = next_envelope(&inner, event);
            let encoded_len = encode_event(&envelope)?.len();
            if inner.bytes + encoded_len > MAX_RUNNER_SPOOL_BYTES {
                return Err(Error::Task(
                    "terminal event does not fit the bounded spool".into(),
                ));
            }
            let envelope = append_encoded(&mut inner, envelope, true).await?;
            if terminal {
                inner.terminal_sequence = Some(envelope.sequence);
            }
            envelope
        };
        let sequence = published.sequence;
        let _ = self.events.send(published);
        Ok(sequence)
    }
}

fn next_envelope(inner: &LogInner, event: RunnerEvent) -> RunnerEventEnvelope {
    RunnerEventEnvelope {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence: inner.head + 1,
        occurred_at_ms: now_ms(),
        event,
    }
}

fn encode_event(event: &RunnerEventEnvelope) -> Result<Vec<u8>, Error> {
    let mut encoded = serde_json::to_vec(event)?;
    if encoded.len() > MAX_RUNNER_FRAME_BYTES {
        return Err(Error::Task("runner event exceeded the frame limit".into()));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

async fn append_encoded(
    inner: &mut LogInner,
    event: RunnerEventEnvelope,
    sync: bool,
) -> Result<RunnerEventEnvelope, Error> {
    let encoded = encode_event(&event)?;
    inner.file.write_all(&encoded).await?;
    inner.file.flush().await?;
    if sync {
        inner.file.get_ref().sync_data().await?;
    }
    inner.bytes += encoded.len();
    inner.head = event.sequence;
    Ok(event)
}

fn now_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
