//! File outbox for sandboxed providers.
//!
//! A sandboxed provider's Unix-socket connect to the daemon can fail from
//! inside the agent's own shell-tool call (Codex's `workspace-write`
//! sandbox: `Operation not permitted`) even though the same daemon's
//! *hooks* -- run unsandboxed, as the provider's own hook subprocess, not a
//! sandboxed tool call -- always get through. This module is Dark Factory's
//! Munder-Difflin-shaped fix: instead of trying to punch a socket-shaped
//! hole through the sandbox, an agent whose `task done`/`task blocked`/
//! `agent message` cannot reach the daemon writes its intended request as a
//! file in its own directory (`$DARK_FACTORY_AGENT_DIR`, already inside a
//! Codex session's `writable_roots`); the next `factoryctl hook`
//! invocation -- which always runs unsandboxed -- carries it the rest of
//! the way. See `docs/providers.md`'s "Sandboxed providers: the outbox".
//!
//! [`queue`] is called from `main.rs`'s outbox-eligible commands on a
//! connect/send failure (or when `DARK_FACTORY_FORCE_OUTBOX=1`, for tests);
//! [`drain`] is called from `main.rs`'s `hook` subcommand before every hook
//! request.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use factory_core::local::{LocalRequest, LocalResponse, ServerFrame};
use factoryctl::Client;
use uuid::Uuid;

const OUTBOX_DIR_NAME: &str = "outbox";
const OUTBOX_DIR_MODE: u32 = 0o700;
const OUTBOX_FILE_MODE: u32 = 0o600;

/// Bounds one `factoryctl hook` invocation's drain: a burst of queued
/// requests (or a poisoned outbox) must never make the hook itself take an
/// unbounded number of round trips.
const MAX_DRAIN_FILES: usize = 100;
/// Per-file request timeout while draining: short enough that an
/// unreachable or wedged daemon is detected quickly and the remaining
/// files are left for the next hook, comfortably inside [`DRAIN_BUDGET`].
const DRAIN_REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
/// Total wall-clock budget for one drain pass, separate from and in
/// addition to `main.rs`'s own `HOOK_REQUEST_TIMEOUT` for the hook request
/// itself -- so a large or wedged outbox can never make `factoryctl hook`
/// visibly stall the operator's live provider session.
const DRAIN_BUDGET: Duration = Duration::from_secs(3);

fn outbox_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(OUTBOX_DIR_NAME)
}

/// Writes `request` as a new file under `<agent_dir>/outbox/`, named
/// `<unix_ms>-<8 hex>.json` so files drain in submission order and
/// (short of a same-millisecond hex collision, astronomically unlikely)
/// never collide. The directory is created on demand (`0700`); the file
/// itself is written via temp-file-then-rename (`0600`) so a concurrent
/// `factoryctl hook` drain never observes a partially written file.
///
/// # Errors
///
/// Returns any I/O error from creating the outbox directory, serializing
/// `request`, writing the temp file, or renaming it into place.
pub fn queue(agent_dir: &Path, request: &LocalRequest) -> io::Result<PathBuf> {
    let dir = outbox_dir(agent_dir);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(OUTBOX_DIR_MODE)
        .create(&dir)?;
    let name = format!("{}-{}.json", unix_millis(), short_hex());
    let final_path = dir.join(&name);
    let temp_path = dir.join(format!(".{name}.tmp"));
    let payload = serde_json::to_vec(request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let write_result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(OUTBOX_FILE_MODE)
            .open(&temp_path)?;
        file.write_all(&payload)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    fs::rename(&temp_path, &final_path)?;
    Ok(final_path)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn short_hex() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// Drains `<agent_dir>/outbox/*.json` in name order (submission order,
/// since names are `<unix_ms>-<hex>.json`): sends each queued request to
/// the daemon, deleting the file on success or on a daemon-side error
/// (poison-pill avoidance -- a request the daemon has durably rejected,
/// e.g. `Conflict`, can never succeed by retrying it verbatim; the
/// rejection is logged to stderr so it is not silently lost). A transport
/// failure (the daemon is still unreachable) stops the drain immediately,
/// leaving that file and everything after it queued for the next hook
/// invocation. A file that is not valid JSON, or not a valid
/// [`LocalRequest`], can never round-trip through the daemon either and is
/// discarded the same way, logged to stderr.
///
/// Bounded to [`MAX_DRAIN_FILES`] files and [`DRAIN_BUDGET`] wall-clock
/// time; a missing or unreadable outbox directory (nothing ever queued) is
/// a silent no-op.
pub fn drain(client: &Client, agent_dir: &Path) {
    let dir = outbox_dir(agent_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect();
    files.sort();
    let deadline = Instant::now() + DRAIN_BUDGET;
    for path in files.into_iter().take(MAX_DRAIN_FILES) {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(request) = serde_json::from_slice::<LocalRequest>(&bytes) else {
            eprintln!(
                "factoryctl: outbox file {} is not a valid queued request; discarding",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        };
        match client.request_with_timeout(request, DRAIN_REQUEST_TIMEOUT) {
            Ok(ServerFrame::Response {
                response: LocalResponse::Error { code, message },
                ..
            }) => {
                eprintln!(
                    "factoryctl: outbox file {} rejected by the daemon ({code:?}: {message}); discarding",
                    path.display()
                );
                let _ = fs::remove_file(&path);
            }
            Ok(_) => {
                let _ = fs::remove_file(&path);
            }
            Err(_) => {
                // Transport failure: the daemon is still (or newly)
                // unreachable. Leave this file and everything after it for
                // the next hook rather than burning each one's own
                // timeout for nothing.
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        io::{BufRead, BufReader},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        thread,
    };

    use factory_core::{PROTOCOL_VERSION, ProjectId, TaskId};

    use super::*;

    fn sample_request() -> LocalRequest {
        LocalRequest::CompleteTask {
            project_id: ProjectId::try_from("factory").unwrap(),
            task_id: TaskId::try_from("task-1").unwrap(),
            result: "done".to_owned(),
        }
    }

    #[test]
    fn queue_creates_a_private_directory_and_file() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        let path = queue(&agent_dir, &sample_request()).unwrap();

        let dir_mode = fs::metadata(outbox_dir(&agent_dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);

        let contents = fs::read_to_string(&path).unwrap();
        let parsed: LocalRequest = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed, sample_request());
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(".json")
        );
    }

    #[test]
    fn queue_never_collides_across_rapid_calls() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        let mut paths = HashSet::new();
        for _ in 0..20 {
            paths.insert(queue(&agent_dir, &sample_request()).unwrap());
        }
        assert_eq!(paths.len(), 20);
    }

    #[test]
    fn drain_on_a_missing_outbox_directory_is_a_silent_no_op() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        let client = Client::new(directory.path().join("does-not-exist.sock"));
        drain(&client, &agent_dir); // must not panic, must not create anything
        assert!(!outbox_dir(&agent_dir).exists());
    }

    #[test]
    fn drain_leaves_files_queued_when_the_daemon_is_unreachable() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        queue(&agent_dir, &sample_request()).unwrap();
        queue(&agent_dir, &sample_request()).unwrap();
        let client = Client::new(directory.path().join("does-not-exist.sock"));

        drain(&client, &agent_dir);

        let remaining = fs::read_dir(outbox_dir(&agent_dir)).unwrap().count();
        assert_eq!(remaining, 2);
    }

    #[test]
    fn drain_discards_a_malformed_file_rather_than_retrying_it_forever() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        let dir = outbox_dir(&agent_dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1-deadbeef.json"), b"not json").unwrap();
        let client = Client::new(directory.path().join("does-not-exist.sock"));

        drain(&client, &agent_dir);

        assert!(fs::read_dir(&dir).unwrap().next().is_none());
    }

    /// Replies to exactly `responses.len()` sequential connections (matching
    /// `Client`'s one-connection-per-request shape) with the given
    /// [`ServerFrame`]s in order, then stops.
    fn spawn_mock_daemon(
        socket_path: PathBuf,
        responses: Vec<ServerFrame>,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(&socket_path).unwrap();
        thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                respond_once(stream, &response);
            }
        })
    }

    fn respond_once(mut stream: UnixStream, response: &ServerFrame) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let mut payload = serde_json::to_vec(response).unwrap();
        payload.push(b'\n');
        let _ = stream.write_all(&payload);
    }

    fn ok_frame() -> ServerFrame {
        ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::TaskCompleted {
                task: sample_task_detail(),
            },
        }
    }

    fn conflict_frame() -> ServerFrame {
        ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::Error {
                code: factory_core::local::ErrorCode::Conflict,
                message: "task already has no open episode".to_owned(),
            },
        }
    }

    fn sample_task_detail() -> factory_core::TaskDetail {
        factory_core::TaskDetail {
            snapshot: factory_core::TaskSnapshot {
                id: TaskId::try_from("task-1").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                title: "Task".to_owned(),
                worktree_binding: None,
                priority: 0,
                status: factory_core::TaskStatus::Succeeded,
                assigned_agent_id: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            body: "body".to_owned(),
            result: Some("done".to_owned()),
            blocked_reason: None,
        }
    }

    #[test]
    fn drain_deletes_the_file_on_a_successful_response() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        queue(&agent_dir, &sample_request()).unwrap();
        let socket = directory.path().join("f.sock");
        let server = spawn_mock_daemon(socket.clone(), vec![ok_frame()]);
        let client = Client::new(&socket);

        drain(&client, &agent_dir);
        server.join().unwrap();

        assert!(
            fs::read_dir(outbox_dir(&agent_dir))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn drain_deletes_a_daemon_rejected_file_and_continues_to_the_next() {
        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        // Two files, drained in name order: the first must be discarded on
        // a daemon-side Conflict without aborting the whole drain, so the
        // second still gets its chance.
        let first = queue(&agent_dir, &sample_request()).unwrap();
        thread::sleep(Duration::from_millis(2));
        let second = queue(&agent_dir, &sample_request()).unwrap();
        assert!(first < second);
        let socket = directory.path().join("f.sock");
        let server = spawn_mock_daemon(socket.clone(), vec![conflict_frame(), ok_frame()]);
        let client = Client::new(&socket);

        drain(&client, &agent_dir);
        server.join().unwrap();

        assert!(
            fs::read_dir(outbox_dir(&agent_dir))
                .unwrap()
                .next()
                .is_none()
        );
    }
}
