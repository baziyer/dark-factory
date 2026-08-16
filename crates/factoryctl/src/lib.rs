//! Blocking client for Dark Factory's local Unix socket.

use std::{
    error::Error,
    fmt,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use factory_core::{
    PROTOCOL_VERSION,
    local::{LocalRequest, RequestEnvelope, ServerFrame},
};

pub use factory_core::local::MAX_LOCAL_FRAME_BYTES as MAX_FRAME_BYTES;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Json(serde_json::Error),
    FrameTooLarge { max: usize },
    UnexpectedEof,
    UnexpectedEvent,
    UnsupportedProtocol { found: u16, supported: u16 },
    InconsistentEventProtocol { frame: u16, event: u16 },
    Disconnected { after_sequence: i64 },
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => write!(formatter, "invalid JSON frame: {error}"),
            Self::FrameTooLarge { max } => {
                write!(formatter, "local protocol frame exceeds {max} bytes")
            }
            Self::UnexpectedEof => formatter.write_str("local protocol frame ended before newline"),
            Self::UnexpectedEvent => {
                formatter.write_str("daemon sent an event where a response was required")
            }
            Self::UnsupportedProtocol { found, supported } => write!(
                formatter,
                "daemon protocol version {found} is not supported (expected {supported})"
            ),
            Self::InconsistentEventProtocol { frame, event } => write!(
                formatter,
                "server frame protocol version {frame} does not match event version {event}"
            ),
            Self::Disconnected { after_sequence } => write!(
                formatter,
                "event stream disconnected; resume after sequence {after_sequence}"
            ),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    #[must_use]
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
        }
    }

    pub fn request(&self, request: LocalRequest) -> Result<ServerFrame, ClientError> {
        self.request_with_timeout(request, REQUEST_TIMEOUT)
    }

    /// Like [`Self::request`] but with an explicit read/write timeout
    /// instead of the default 15 seconds — e.g. `factoryctl hook`'s 5-second
    /// fail-open budget, so a slow or wedged daemon never blocks a live
    /// provider hook invocation.
    pub fn request_with_timeout(
        &self,
        request: LocalRequest,
        timeout: Duration,
    ) -> Result<ServerFrame, ClientError> {
        let stream = self.connect_with_timeout(request, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        let mut reader = BufReader::new(stream);
        let frame = read_frame(&mut reader)?.ok_or(ClientError::UnexpectedEof)?;
        validate_frame(&frame)?;
        if matches!(frame, ServerFrame::Event { .. }) {
            return Err(ClientError::UnexpectedEvent);
        }
        Ok(frame)
    }

    pub fn subscribe(&self, after_sequence: i64) -> Result<Subscription, ClientError> {
        let stream = self.connect(LocalRequest::Subscribe { after_sequence })?;
        Ok(Subscription {
            reader: BufReader::new(stream),
            finished: false,
            after_sequence,
        })
    }

    /// Opens a persistent connection for an `AttachTerminal` request and
    /// returns every frame the daemon sends on it (retained-then-live
    /// `ServerFrame::TerminalOutput` for the attached run, or an error
    /// response). The connection stays open, unbounded by any read timeout,
    /// until the returned iterator is dropped or the daemon closes it.
    pub fn attach_terminal(&self, request: LocalRequest) -> Result<TerminalFrames, ClientError> {
        let stream = self.connect(request)?;
        Ok(TerminalFrames {
            reader: BufReader::new(stream),
            finished: false,
        })
    }

    fn connect(&self, request: LocalRequest) -> Result<UnixStream, ClientError> {
        self.connect_with_timeout(request, REQUEST_TIMEOUT)
    }

    fn connect_with_timeout(
        &self,
        request: LocalRequest,
        timeout: Duration,
    ) -> Result<UnixStream, ClientError> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(timeout))?;
        write_request(&mut stream, &RequestEnvelope::new(request))?;
        Ok(stream)
    }
}

pub struct Subscription {
    reader: BufReader<UnixStream>,
    finished: bool,
    after_sequence: i64,
}

impl Iterator for Subscription {
    type Item = Result<ServerFrame, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match read_frame(&mut self.reader) {
            Ok(Some(frame)) => match validate_frame(&frame) {
                Ok(()) => {
                    if let ServerFrame::Event { event, .. } = &frame {
                        self.after_sequence = self.after_sequence.max(event.sequence);
                    }
                    Some(Ok(frame))
                }
                Err(error) => {
                    self.finished = true;
                    Some(Err(error))
                }
            },
            Ok(None) => {
                self.finished = true;
                Some(Err(ClientError::Disconnected {
                    after_sequence: self.after_sequence,
                }))
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

/// Frames from one `AttachTerminal` connection. See [`Client::attach_terminal`].
pub struct TerminalFrames {
    reader: BufReader<UnixStream>,
    finished: bool,
}

impl Iterator for TerminalFrames {
    type Item = Result<ServerFrame, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match read_frame(&mut self.reader) {
            Ok(Some(frame)) => match validate_frame(&frame) {
                Ok(()) => Some(Ok(frame)),
                Err(error) => {
                    self.finished = true;
                    Some(Err(error))
                }
            },
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

fn write_request(writer: &mut impl Write, request: &RequestEnvelope) -> Result<(), ClientError> {
    let payload = serde_json::to_vec(request)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ClientError::FrameTooLarge {
            max: MAX_FRAME_BYTES,
        });
    }
    writer.write_all(&payload)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<ServerFrame>, ClientError> {
    let mut payload = Vec::new();
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if payload.is_empty() {
                    return Ok(None);
                }
                return Err(ClientError::UnexpectedEof);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            let content_len = newline.unwrap_or(available.len());
            if payload.len() + content_len > MAX_FRAME_BYTES {
                return Err(ClientError::FrameTooLarge {
                    max: MAX_FRAME_BYTES,
                });
            }
            payload.extend_from_slice(&available[..content_len]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if complete {
            break;
        }
    }
    if payload.last() == Some(&b'\r') {
        payload.pop();
    }
    Ok(Some(serde_json::from_slice(&payload)?))
}

fn validate_frame(frame: &ServerFrame) -> Result<(), ClientError> {
    let protocol_version = frame.protocol_version();
    if protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::UnsupportedProtocol {
            found: protocol_version,
            supported: PROTOCOL_VERSION,
        });
    }
    if let ServerFrame::Event { event, .. } = frame {
        if event.protocol_version != protocol_version {
            return Err(ClientError::InconsistentEventProtocol {
                frame: protocol_version,
                event: event.protocol_version,
            });
        }
    }
    Ok(())
}
