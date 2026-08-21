# Installation status

Live use remains frozen until an independent exact-main boot review passes.

Do not install, update, or start a development revision. In particular, do
not:

- run `factoryctl init` or `factoryctl update --install` from this checkout;
- replace binaries under `~/.dark-factory/bin`;
- load or restart the installed launchd job;
- migrate the operator database; or
- use the operator home or socket for a test.

Developers use a temporary `DARK_FACTORY_HOME` and explicit socket as described
in the [development workflow](development/WORKFLOW.md). A prior release may
remain installed but must stay stopped when directed by the operator.

Boot approval does not itself install, release, start, enable dispatch, or
modify the operator system. Each is a separate explicit decision; supported
installation instructions return only after those decisions establish a live
release.
