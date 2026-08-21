# macOS service

This directory contains the LaunchAgent template used by Dark Factory's
managed macOS installation. The service keeps `factoryd` running independently
of `factoryctl` and `factory-tui`.

Managed installation and service changes are currently paused. Do not render,
install, reload, or remove the LaunchAgent from current `main`, and do not use
the operator's `~/.dark-factory` as a development fixture. Supported service
and uninstall steps will return here when managed installation resumes.

Development daemons run directly with a temporary `DARK_FACTORY_HOME` and an
explicit private socket. See the
[development workflow](../docs/development/WORKFLOW.md) for the safe command
sequence.
