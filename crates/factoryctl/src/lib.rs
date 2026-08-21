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
    local::{LocalRequest, RequestCredential, RequestEnvelope, ServerFrame},
};

pub mod capacity;
pub mod install;
pub mod launchd;
pub mod managed_update;
pub mod probes;
pub mod runtime;
pub mod update;

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
                "server frame protocol version {frame} cannot carry newer event version {event}"
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
    credential: Option<RequestCredential>,
}

impl Client {
    #[must_use]
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
            credential: None,
        }
    }

    /// Creates a client whose ordinary requests and subscriptions carry the
    /// supplied bearer. The bearer is redacted by its `Debug` implementation.
    #[must_use]
    pub fn authenticated(socket: impl AsRef<Path>, credential: RequestCredential) -> Self {
        Self {
            socket: socket.as_ref().to_owned(),
            credential: Some(credential),
        }
    }

    pub fn authenticated_from_file(
        socket: impl AsRef<Path>,
        credential_file: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let value = std::fs::read_to_string(credential_file)?;
        let credential = RequestCredential::new(value.trim().to_owned())
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
        Ok(Self::authenticated(socket, credential))
    }

    pub fn request(&self, request: LocalRequest) -> Result<ServerFrame, ClientError> {
        self.request_with_timeout(request, REQUEST_TIMEOUT)
    }

    /// Sends one request with an opaque bearer. The caller chooses only the
    /// credential value; factoryd resolves its principal and owned attempt.
    pub fn request_authenticated(
        &self,
        request: LocalRequest,
        credential: RequestCredential,
    ) -> Result<ServerFrame, ClientError> {
        self.request_with_timeout_authenticated(request, credential, REQUEST_TIMEOUT)
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
        self.request_envelope_with_timeout(self.envelope(request), timeout)
    }

    pub fn request_with_timeout_authenticated(
        &self,
        request: LocalRequest,
        credential: RequestCredential,
        timeout: Duration,
    ) -> Result<ServerFrame, ClientError> {
        self.request_envelope_with_timeout(
            RequestEnvelope::authenticated(request, credential),
            timeout,
        )
    }

    fn request_envelope_with_timeout(
        &self,
        envelope: RequestEnvelope,
        timeout: Duration,
    ) -> Result<ServerFrame, ClientError> {
        let stream = self.connect_envelope_with_timeout(envelope, timeout)?;
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

    fn connect(&self, request: LocalRequest) -> Result<UnixStream, ClientError> {
        self.connect_with_timeout(request, REQUEST_TIMEOUT)
    }

    fn connect_with_timeout(
        &self,
        request: LocalRequest,
        timeout: Duration,
    ) -> Result<UnixStream, ClientError> {
        self.connect_envelope_with_timeout(self.envelope(request), timeout)
    }

    fn envelope(&self, request: LocalRequest) -> RequestEnvelope {
        match &self.credential {
            Some(credential) => RequestEnvelope::authenticated(request, credential.clone()),
            None => RequestEnvelope::new(request),
        }
    }

    fn connect_envelope_with_timeout(
        &self,
        envelope: RequestEnvelope,
        timeout: Duration,
    ) -> Result<UnixStream, ClientError> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(timeout))?;
        write_request(&mut stream, &envelope)?;
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
        // Durable events retain the schema version with which they were
        // written. A v1 event replayed by the v2 daemon is valid; only an
        // event newer than the frame can be unsafe to decode.
        if event.protocol_version > protocol_version {
            return Err(ClientError::InconsistentEventProtocol {
                frame: protocol_version,
                event: event.protocol_version,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use factory_core::{FactoryEvent, ProjectId, ProjectSnapshot};

    #[test]
    fn v1_durable_event_is_accepted_inside_a_v2_replay_frame() {
        let frame = ServerFrame::Event {
            protocol_version: PROTOCOL_VERSION,
            event: factory_core::EventEnvelope {
                protocol_version: 1,
                sequence: 1,
                occurred_at_ms: 1,
                event: FactoryEvent::ProjectChanged {
                    project: ProjectSnapshot {
                        id: ProjectId::try_from("project").unwrap(),
                        name: "Project".into(),
                        root: "/tmp/project".into(),
                        completion_verification: factory_core::CompletionVerification::None,
                        created_at_ms: 1,
                        updated_at_ms: 1,
                    },
                },
            },
        };
        assert!(validate_frame(&frame).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        thread,
    };

    use factory_core::{
        AgentId, EventEnvelope, ExecutionMode, FactoryEvent, ProjectId,
        local::{ErrorCode, LocalResponse},
    };

    use super::*;

    #[test]
    fn policy_mutation_rejects_an_old_daemon_instead_of_dropping_fields() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let envelope: factory_core::local::RequestEnvelope =
                serde_json::from_str(&line).unwrap();
            assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
            match envelope.request {
                LocalRequest::UpdateAgentProfile {
                    model,
                    reasoning_effort,
                    model_selection_reason,
                    ..
                } => {
                    assert_eq!(model.as_deref(), Some("gpt-5.6-sol"));
                    assert_eq!(reasoning_effort.as_deref(), Some("xhigh"));
                    assert_eq!(
                        model_selection_reason.as_deref(),
                        Some("release integration")
                    );
                }
                request => panic!("unexpected request: {request:?}"),
            }
            let old_frame = ServerFrame::Response {
                protocol_version: 1,
                response: LocalResponse::Error {
                    code: ErrorCode::UnsupportedProtocol,
                    message: "old daemon".into(),
                },
            };
            let payload = serde_json::to_vec(&old_frame).unwrap();
            stream.write_all(&payload).unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let error = Client::new(&socket)
            .request(LocalRequest::UpdateAgentProfile {
                project_id: ProjectId::try_from("factory").unwrap(),
                agent_id: AgentId::try_from("worker").unwrap(),
                model: Some("gpt-5.6-sol".into()),
                reasoning_effort: Some("xhigh".into()),
                model_selection_reason: Some("release integration".into()),
                execution_mode: ExecutionMode::WorkspaceWrite,
                instructions: String::new(),
                memory: String::new(),
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::UnsupportedProtocol { found: 1, supported }
                if supported == PROTOCOL_VERSION
        ));
        server.join().unwrap();
    }

    #[test]
    fn subscription_accepts_stored_v1_replay_inside_v2_frames() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let envelope: factory_core::local::RequestEnvelope =
                serde_json::from_str(&line).unwrap();
            assert!(matches!(
                envelope.request,
                LocalRequest::Subscribe { after_sequence: 0 }
            ));
            let frames = [
                ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::Subscribed {
                        after_sequence: 0,
                        replay_through: 1,
                    },
                },
                ServerFrame::Event {
                    protocol_version: PROTOCOL_VERSION,
                    event: EventEnvelope {
                        protocol_version: 1,
                        sequence: 1,
                        occurred_at_ms: 10,
                        event: FactoryEvent::LegacyAutoModeChanged { enabled: true },
                    },
                },
                ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::CaughtUp { sequence: 1 },
                },
                ServerFrame::Event {
                    protocol_version: PROTOCOL_VERSION,
                    event: EventEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        sequence: 2,
                        occurred_at_ms: 11,
                        event: FactoryEvent::DispatchPolicyChanged { enabled: false },
                    },
                },
            ];
            for frame in frames {
                serde_json::to_writer(&mut stream, &frame).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });

        let mut subscription = Client::new(&socket).subscribe(0).unwrap();
        assert!(matches!(
            subscription.next().unwrap().unwrap(),
            ServerFrame::Response {
                response: LocalResponse::Subscribed {
                    replay_through: 1,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            subscription.next().unwrap().unwrap(),
            ServerFrame::Event {
                event: EventEnvelope {
                    protocol_version: 1,
                    sequence: 1,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            subscription.next().unwrap().unwrap(),
            ServerFrame::Response {
                response: LocalResponse::CaughtUp { sequence: 1 },
                ..
            }
        ));
        assert!(matches!(
            subscription.next().unwrap().unwrap(),
            ServerFrame::Event {
                event: EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence: 2,
                    ..
                },
                ..
            }
        ));
        server.join().unwrap();
    }
}
