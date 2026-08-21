use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use factory_core::local::{
    ErrorCode, LocalRequest, LocalResponse, RequestCredential, RequestEnvelope, ServerFrame,
};
use factoryd::{daemon_state::DaemonState, execution, local_api, store::Store};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

async fn request(socket: &Path, envelope: RequestEnvelope) -> ServerFrame {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let mut encoded = serde_json::to_vec(&envelope).unwrap();
    encoded.push(b'\n');
    stream.write_all(&encoded).await.unwrap();
    let mut line = Vec::new();
    BufReader::new(stream)
        .read_until(b'\n', &mut line)
        .await
        .unwrap();
    serde_json::from_slice(&line).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_authority_is_explicit_and_taskless_bearers_are_refused() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("factory.sock");
    let runtime_root = directory.path().join("runs");
    let state = DaemonState::new(Store::open_in_memory().unwrap());
    let (execution, manager) = execution::spawn(
        execution::Config {
            factoryd_program: "/bin/false".into(),
            runner_program: "/bin/false".into(),
            factoryctl_path: "/bin/false".into(),
            git_program: "/bin/false".into(),
            claude_installation: None,
            cargo_program: Some("/bin/false".into()),
            runtime_root,
            changes_root: directory.path().join("changes"),
            artifacts_root: directory.path().join("artifacts"),
            guidance_root: directory.path().join("guidance"),
            socket_path: socket.clone(),
            max_active_runs: 1,
        },
        state.clone(),
    )
    .unwrap();
    let operator = RequestCredential::new("operator-secret".into()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let (stop, shutdown) = oneshot::channel();
    let server = tokio::spawn(local_api::serve(
        listener,
        state,
        execution.clone(),
        directory.path().join("guidance"),
        operator.clone(),
        async move {
            let _ = shutdown.await;
        },
    ));

    assert!(matches!(
        request(&socket, RequestEnvelope::new(LocalRequest::Health)).await,
        ServerFrame::Response {
            response: LocalResponse::Health { .. },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::new(LocalRequest::ListProjects {
                after_id: None,
                limit: 1,
            }),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::new(LocalRequest::RustStorageStatus),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(LocalRequest::RustStorageStatus, operator.clone()),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::RustStorageStatus { storage },
            ..
        } if storage.cache_count == 0
            && storage.cache_bytes == Some(0)
            && storage.complete
    ));

    let taskless = RequestCredential::new("not-an-admitted-attempt".into()).unwrap();
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::CompleteAttempt {
                    result: "done".into(),
                },
                taskless,
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::CompleteAttempt {
                    result: "done".into(),
                },
                operator.clone(),
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::ListProjects {
                    after_id: None,
                    limit: 1,
                },
                operator,
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Projects { projects, .. },
            ..
        } if projects.is_empty()
    ));

    let _ = stop.send(());
    server.await.unwrap().unwrap();
    execution.shutdown().await.unwrap();
    manager.await.unwrap().unwrap();
}
