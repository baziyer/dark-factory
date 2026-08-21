//! Daemon state and supervision.

mod change_source;
pub mod daemon_state;
pub mod execution;
pub mod guidance;
pub mod lifecycle;
pub mod local_api;
pub mod policy;
pub mod providers;
pub mod runner_client;
pub mod runner_process;
pub mod store;
pub mod webhook_http;

/// Internal entrypoint used only when the daemon binary has been launched as
/// the registered source-materializer wrapper.
#[doc(hidden)]
pub fn run_change_materializer(
    invocation: &std::path::Path,
) -> Result<std::convert::Infallible, String> {
    change_source::run_materializer_invocation(invocation).map_err(|error| error.to_string())
}
