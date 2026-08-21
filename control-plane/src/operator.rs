//! Reserved operator/PWA namespace. The future surface will expose a bounded,
//! authenticated projection and commands, never GitHub credentials or raw
//! delivery payloads.

pub const API_PREFIX: &str = "/v1/operator";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatorApiState {
    Inactive,
}
