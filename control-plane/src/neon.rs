use std::time::{Duration, Instant};

use reqwest::{
    Client,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use crate::journal::{NeonIdentity, RUNTIME_ROLE};

const API_BASE: &str = "https://console.neon.tech/api/v2/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATIONS_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Configuration,
    Rejected,
    IndeterminateReset,
}

pub(crate) struct NeonApi {
    client: Client,
    base: Url,
    operations_timeout: Duration,
    poll_interval: Duration,
}

impl NeonApi {
    pub(crate) fn new(api_key: &str) -> Result<Self, Error> {
        Self::with_base(
            api_key,
            Url::parse(API_BASE).map_err(|_| Error::Configuration)?,
            OPERATIONS_TIMEOUT,
            POLL_INTERVAL,
        )
    }

    fn with_base(
        api_key: &str,
        base: Url,
        operations_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, Error> {
        if api_key.is_empty()
            || api_key.len() > 1024
            || api_key
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(Error::Configuration);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| Error::Configuration)?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let client = Client::builder()
            .default_headers(headers)
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| Error::Configuration)?;
        Ok(Self {
            client,
            base,
            operations_timeout,
            poll_interval,
        })
    }

    pub(crate) async fn reset_runtime_password(
        &self,
        identity: &NeonIdentity,
    ) -> Result<String, Error> {
        let role_endpoint = self.endpoint(&[
            "projects",
            &identity.project_id,
            "branches",
            &identity.branch_id,
            "roles",
            RUNTIME_ROLE,
        ])?;
        let role = self
            .client
            .get(role_endpoint.clone())
            .send()
            .await
            .map_err(|_| Error::Rejected)?;
        if !role.status().is_success() {
            return Err(Error::Rejected);
        }
        let role = decode_json::<RoleResponse>(role)
            .await
            .map_err(|_| Error::Rejected)?;
        if !role.role.is_compatible(identity) {
            return Err(Error::Rejected);
        }
        let mut endpoint = role_endpoint;
        endpoint
            .path_segments_mut()
            .map_err(|_| Error::Configuration)?
            .push("reset_password");
        // This non-idempotent request is deliberately issued exactly once.
        // A transport failure or malformed success response may mean Neon
        // rotated the password without returning it, so it is indeterminate.
        let response = self
            .client
            .post(endpoint)
            .send()
            .await
            .map_err(|_| Error::IndeterminateReset)?;
        if response.status().is_server_error() {
            return Err(Error::IndeterminateReset);
        }
        if !response.status().is_success() {
            return Err(Error::Rejected);
        }
        let reset = decode_json::<ResetResponse>(response)
            .await
            .map_err(|_| Error::IndeterminateReset)?;
        let password = reset.validate(identity).ok_or(Error::IndeterminateReset)?;
        self.wait_for_operations(identity, &reset.operations)
            .await?;
        Ok(password.to_owned())
    }

    async fn wait_for_operations(
        &self,
        identity: &NeonIdentity,
        operations: &[Operation],
    ) -> Result<(), Error> {
        if operations.is_empty() {
            return Err(Error::IndeterminateReset);
        }
        let deadline = Instant::now() + self.operations_timeout;
        for operation in operations {
            if !operation.is_valid_for(identity) {
                return Err(Error::IndeterminateReset);
            }
            let mut status = operation.status;
            while !status.is_terminal() {
                if Instant::now() >= deadline {
                    return Err(Error::IndeterminateReset);
                }
                tokio::time::sleep(self.poll_interval).await;
                let endpoint = self
                    .endpoint(&[
                        "projects",
                        &identity.project_id,
                        "operations",
                        &operation.id,
                    ])
                    .map_err(|_| Error::IndeterminateReset)?;
                let response = self
                    .client
                    .get(endpoint)
                    .send()
                    .await
                    .map_err(|_| Error::IndeterminateReset)?;
                if !response.status().is_success() {
                    return Err(Error::IndeterminateReset);
                }
                let polled = decode_json::<OperationResponse>(response)
                    .await
                    .map_err(|_| Error::IndeterminateReset)?
                    .operation;
                if polled.id != operation.id {
                    return Err(Error::IndeterminateReset);
                }
                if !polled.is_valid_for(identity) {
                    return Err(Error::IndeterminateReset);
                }
                status = polled.status;
            }
            if status != OperationStatus::Finished {
                return Err(Error::IndeterminateReset);
            }
        }
        Ok(())
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, Error> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| Error::Configuration)?
            .extend(segments);
        Ok(endpoint)
    }
}

async fn decode_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, ()> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| ())
}

#[derive(Deserialize)]
struct ResetResponse {
    role: ResetRole,
    operations: Vec<Operation>,
}

impl ResetResponse {
    fn validate(&self, identity: &NeonIdentity) -> Option<&str> {
        let password = self.role.password.as_deref()?;
        (self.role.is_compatible(identity) && !password.is_empty() && password.len() <= 1024)
            .then_some(password)
    }
}

#[derive(Deserialize)]
struct ResetRole {
    branch_id: String,
    name: String,
    password: Option<String>,
    protected: Option<bool>,
}

impl ResetRole {
    fn is_compatible(&self, identity: &NeonIdentity) -> bool {
        self.branch_id == identity.branch_id
            && self.name == RUNTIME_ROLE
            && self.protected != Some(true)
    }
}

#[derive(Deserialize)]
struct RoleResponse {
    role: ResetRole,
}

#[derive(Deserialize)]
struct OperationResponse {
    operation: Operation,
}

#[derive(Deserialize)]
struct Operation {
    id: String,
    project_id: String,
    branch_id: Option<String>,
    status: OperationStatus,
}

impl Operation {
    fn is_valid_for(&self, identity: &NeonIdentity) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 128
            && self.project_id == identity.project_id
            && self
                .branch_id
                .as_deref()
                .is_none_or(|branch| branch == identity.branch_id)
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OperationStatus {
    Scheduling,
    Running,
    Finished,
    Failed,
    Error,
    Cancelling,
    Cancelled,
    Skipped,
}

impl OperationStatus {
    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Finished | Self::Failed | Self::Error | Self::Cancelled | Self::Skipped
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        thread,
    };

    use super::*;

    const PROJECT: &str = "withered-mouse-49434395";
    const BRANCH: &str = "br-curly-sky-zaol0ck0";
    const OPERATION: &str = "6bef07a0-ebca-40cd-9100-7324036cfff2";

    #[tokio::test]
    async fn typed_reset_waits_for_the_exact_operation() {
        let responses = [
            response("200 OK", &role_body()),
            response(
                "200 OK",
                &reset_body("running", "generated-runtime-password"),
            ),
            response("200 OK", &operation_body("finished")),
        ];
        let (base, server) = mock_server(responses);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(2),
            Duration::from_millis(1),
        )
        .unwrap();
        let password = api.reset_runtime_password(&identity()).await.unwrap();
        assert_eq!(password, "generated-runtime-password");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejected_and_indeterminate_posts_are_never_retried() {
        let (base, rejected_server) = mock_server([
            response("200 OK", &role_body()),
            response("403 Forbidden", "{}"),
        ]);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::Rejected)
        );
        rejected_server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let indeterminate_server = thread::spawn(move || {
            for response in [Some(response("200 OK", &role_body())), None] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                if let Some(response) = response {
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        let api = NeonApi::with_base(
            "test-key",
            Url::parse(&format!("http://{address}/api/v2/")).unwrap(),
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::IndeterminateReset)
        );
        indeterminate_server.join().unwrap();
    }

    #[tokio::test]
    async fn server_error_after_reset_post_is_indeterminate() {
        let (base, server) = mock_server([
            response("200 OK", &role_body()),
            response("503 Service Unavailable", "{}"),
        ]);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::IndeterminateReset)
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn every_failure_after_an_accepted_reset_is_indeterminate() {
        let (base, poll_server) = mock_server([
            response("200 OK", &role_body()),
            response(
                "200 OK",
                &reset_body("running", "generated-runtime-password"),
            ),
            response("503 Service Unavailable", "{}"),
        ]);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::IndeterminateReset)
        );
        poll_server.join().unwrap();

        let (base, failed_server) = mock_server([
            response("200 OK", &role_body()),
            response(
                "200 OK",
                &reset_body("failed", "generated-runtime-password"),
            ),
        ]);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::IndeterminateReset)
        );
        failed_server.join().unwrap();

        let (base, timeout_server) = mock_server([
            response("200 OK", &role_body()),
            response(
                "200 OK",
                &reset_body("running", "generated-runtime-password"),
            ),
        ]);
        let api =
            NeonApi::with_base("test-key", base, Duration::ZERO, Duration::from_millis(1)).unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::IndeterminateReset)
        );
        timeout_server.join().unwrap();
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let (base, server) = mock_server([concat!(
            "HTTP/1.1 302 Found\r\n",
            "Location: https://example.invalid/steal\r\n",
            "Content-Length: 0\r\n",
            "Connection: close\r\n\r\n"
        )
        .to_owned()]);
        let api = NeonApi::with_base(
            "test-key",
            base,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            api.reset_runtime_password(&identity()).await,
            Err(Error::Rejected)
        );
        server.join().unwrap();
    }

    fn identity() -> NeonIdentity {
        NeonIdentity {
            project_id: PROJECT.to_owned(),
            branch_id: BRANCH.to_owned(),
        }
    }

    fn reset_body(status: &str, password: &str) -> String {
        format!(
            r#"{{"role":{{"branch_id":"{BRANCH}","name":"{RUNTIME_ROLE}","password":"{password}"}},"operations":[{{"id":"{OPERATION}","project_id":"{PROJECT}","branch_id":"{BRANCH}","status":"{status}"}}]}}"#
        )
    }

    fn role_body() -> String {
        format!(
            r#"{{"role":{{"branch_id":"{BRANCH}","name":"{RUNTIME_ROLE}","protected":false}}}}"#
        )
    }

    fn operation_body(status: &str) -> String {
        format!(
            r#"{{"operation":{{"id":"{OPERATION}","project_id":"{PROJECT}","branch_id":"{BRANCH}","status":"{status}"}}}}"#
        )
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn mock_server<const N: usize>(responses: [String; N]) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (
            Url::parse(&format!("http://{address}/api/v2/")).unwrap(),
            server,
        )
    }
}
