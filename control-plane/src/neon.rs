use std::time::Duration;

use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use crate::journal::{NeonIdentity, RUNTIME_ROLE};

const API_BASE: &str = "https://console.neon.tech/api/v2/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Configuration,
    Rejected,
    PasswordUnavailable,
    IndeterminateReset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PasswordSource {
    Revealed,
    Reset,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RecoveredPassword {
    password: String,
    source: PasswordSource,
}

impl RecoveredPassword {
    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    pub(crate) const fn source(&self) -> PasswordSource {
        self.source
    }
}

pub(crate) struct NeonApi {
    client: Client,
    base: Url,
}

impl NeonApi {
    pub(crate) fn new(api_key: &str) -> Result<Self, Error> {
        Self::with_base(
            api_key,
            Url::parse(API_BASE).map_err(|_| Error::Configuration)?,
        )
    }

    fn with_base(api_key: &str, base: Url) -> Result<Self, Error> {
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
        Ok(Self { client, base })
    }

    pub(crate) async fn recover_runtime_password(
        &self,
        identity: &NeonIdentity,
        reset_if_unavailable: bool,
    ) -> Result<RecoveredPassword, Error> {
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

        let mut reveal_endpoint = role_endpoint.clone();
        reveal_endpoint
            .path_segments_mut()
            .map_err(|_| Error::Configuration)?
            .push("reveal_password");
        let response = self
            .client
            .get(reveal_endpoint)
            .send()
            .await
            .map_err(|_| Error::Rejected)?;
        if response.status().is_success() {
            let password = decode_json::<RolePasswordResponse>(response)
                .await
                .map_err(|_| Error::Rejected)?
                .validate()
                .ok_or(Error::Rejected)?;
            return Ok(RecoveredPassword {
                password,
                source: PasswordSource::Revealed,
            });
        }
        if response.status() != StatusCode::PRECONDITION_FAILED {
            return Err(Error::Rejected);
        }
        if !reset_if_unavailable {
            return Err(Error::PasswordUnavailable);
        }

        let mut reset_endpoint = role_endpoint;
        reset_endpoint
            .path_segments_mut()
            .map_err(|_| Error::Configuration)?
            .push("reset_password");
        // This non-idempotent request is deliberately issued exactly once.
        // Once the typed response is accepted, its password is returned
        // immediately for durable staging. Activation waits separately over
        // fresh database connections instead of risking loss while polling.
        let response = self
            .client
            .post(reset_endpoint)
            .send()
            .await
            .map_err(|_| Error::IndeterminateReset)?;
        if !response.status().is_success() {
            return Err(Error::IndeterminateReset);
        }
        let reset = decode_json::<ResetResponse>(response)
            .await
            .map_err(|_| Error::IndeterminateReset)?;
        let password = reset.validate(identity).ok_or(Error::IndeterminateReset)?;
        Ok(RecoveredPassword {
            password,
            source: PasswordSource::Reset,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, Error> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| Error::Configuration)?
            .pop_if_empty()
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
}

impl ResetResponse {
    fn validate(self, identity: &NeonIdentity) -> Option<String> {
        let role_is_compatible = self.role.is_compatible(identity);
        let password = self.role.password?;
        (role_is_compatible && password_is_valid(&password)).then_some(password)
    }
}

#[derive(Deserialize)]
struct RolePasswordResponse {
    password: String,
}

impl RolePasswordResponse {
    fn validate(self) -> Option<String> {
        password_is_valid(&self.password).then_some(self.password)
    }
}

fn password_is_valid(password: &str) -> bool {
    !password.is_empty()
        && password.len() <= 1024
        && !password.bytes().any(|byte| byte.is_ascii_control())
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
    async fn typed_recovery_reveals_the_existing_password_without_resetting_it() {
        let responses = [
            response("200 OK", &role_body()),
            response("200 OK", &password_body("existing-runtime-password")),
        ];
        let (base, server) = mock_server(responses);
        let api = NeonApi::with_base("test-key", base).unwrap();

        let recovered = api
            .recover_runtime_password(&identity(), false)
            .await
            .unwrap();

        assert_eq!(recovered.password(), "existing-runtime-password");
        assert_eq!(recovered.source(), PasswordSource::Revealed);
        assert_eq!(
            server.join().unwrap(),
            [
                format!(
                    "GET /api/v2/projects/{PROJECT}/branches/{BRANCH}/roles/{RUNTIME_ROLE} HTTP/1.1"
                ),
                format!(
                    "GET /api/v2/projects/{PROJECT}/branches/{BRANCH}/roles/{RUNTIME_ROLE}/reveal_password HTTP/1.1"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn unavailable_reveal_requires_an_explicit_reset_decision() {
        let responses = [
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
        ];
        let (base, server) = mock_server(responses);
        let api = NeonApi::with_base("test-key", base).unwrap();

        assert_eq!(
            api.recover_runtime_password(&identity(), false).await,
            Err(Error::PasswordUnavailable)
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn explicit_fallback_resets_once_and_returns_before_operation_polling() {
        let responses = [
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
            response("200 OK", &reset_body("running", "new-runtime-password")),
        ];
        let (base, server) = mock_server(responses);
        let api = NeonApi::with_base("test-key", base).unwrap();

        let recovered = api
            .recover_runtime_password(&identity(), true)
            .await
            .unwrap();

        assert_eq!(recovered.password(), "new-runtime-password");
        assert_eq!(recovered.source(), PasswordSource::Reset);
        assert_eq!(
            server.join().unwrap(),
            [
                format!(
                    "GET /api/v2/projects/{PROJECT}/branches/{BRANCH}/roles/{RUNTIME_ROLE} HTTP/1.1"
                ),
                format!(
                    "GET /api/v2/projects/{PROJECT}/branches/{BRANCH}/roles/{RUNTIME_ROLE}/reveal_password HTTP/1.1"
                ),
                format!(
                    "POST /api/v2/projects/{PROJECT}/branches/{BRANCH}/roles/{RUNTIME_ROLE}/reset_password HTTP/1.1"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn every_failed_reset_post_is_indeterminate_and_never_retried() {
        let (base, forbidden_server) = mock_server([
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
            response("403 Forbidden", "{}"),
        ]);
        let api = NeonApi::with_base("test-key", base).unwrap();
        assert_eq!(
            api.recover_runtime_password(&identity(), true).await,
            Err(Error::IndeterminateReset)
        );
        forbidden_server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let indeterminate_server = thread::spawn(move || {
            for response in [
                Some(response("200 OK", &role_body())),
                Some(response("412 Precondition Failed", "{}")),
                None,
            ] {
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
        )
        .unwrap();
        assert_eq!(
            api.recover_runtime_password(&identity(), true).await,
            Err(Error::IndeterminateReset)
        );
        indeterminate_server.join().unwrap();
    }

    #[tokio::test]
    async fn server_error_after_reset_post_is_indeterminate() {
        let (base, server) = mock_server([
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
            response("503 Service Unavailable", "{}"),
        ]);
        let api = NeonApi::with_base("test-key", base).unwrap();
        assert_eq!(
            api.recover_runtime_password(&identity(), true).await,
            Err(Error::IndeterminateReset)
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_reset_response_is_indeterminate() {
        let (base, server) = mock_server([
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
            response("200 OK", r#"{"role":{},"operations":[]}"#),
        ]);
        let api = NeonApi::with_base("test-key", base).unwrap();
        assert_eq!(
            api.recover_runtime_password(&identity(), true).await,
            Err(Error::IndeterminateReset)
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn returned_reset_password_is_not_lost_to_operation_state() {
        let body = format!(
            r#"{{"role":{{"branch_id":"{BRANCH}","name":"{RUNTIME_ROLE}","password":"preserved-password"}},"operations":[]}}"#
        );
        let (base, server) = mock_server([
            response("200 OK", &role_body()),
            response("412 Precondition Failed", "{}"),
            response("200 OK", &body),
        ]);
        let api = NeonApi::with_base("test-key", base).unwrap();

        let recovered = api
            .recover_runtime_password(&identity(), true)
            .await
            .unwrap();

        assert_eq!(recovered.password(), "preserved-password");
        assert_eq!(recovered.source(), PasswordSource::Reset);
        server.join().unwrap();
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
        let api = NeonApi::with_base("test-key", base).unwrap();
        assert_eq!(
            api.recover_runtime_password(&identity(), false).await,
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

    fn password_body(password: &str) -> String {
        format!(r#"{{"password":"{password}"}}"#)
    }

    fn role_body() -> String {
        format!(
            r#"{{"role":{{"branch_id":"{BRANCH}","name":"{RUNTIME_ROLE}","protected":false}}}}"#
        )
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn mock_server<const N: usize>(
        responses: [String; N],
    ) -> (Url, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::with_capacity(N);
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let bytes = stream.read(&mut request).unwrap();
                let first_line = String::from_utf8_lossy(&request[..bytes])
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                requests.push(first_line);
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (
            Url::parse(&format!("http://{address}/api/v2/")).unwrap(),
            server,
        )
    }
}
