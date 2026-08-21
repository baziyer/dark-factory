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
mod rust_verify;
pub mod store;

/// Internal entrypoint used only when the daemon binary has been launched as
/// the registered source-materializer wrapper.
#[doc(hidden)]
pub fn run_change_materializer(
    invocation: &std::path::Path,
) -> Result<std::convert::Infallible, String> {
    change_source::run_materializer_invocation(invocation).map_err(|error| error.to_string())
}

/// Internal entrypoint used only by the registered Rust-verifier worker mode.
#[doc(hidden)]
pub fn run_rust_verifier_worker(
    invocation: &std::path::Path,
    result: &std::path::Path,
) -> Result<(), String> {
    rust_verify::run_worker_file(invocation, result).map_err(|error| error.to_string())
}
