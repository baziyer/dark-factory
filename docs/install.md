# Installation frozen

Do not install or update Dark Factory from the safe-kernel refactor. Stage 1 is
an intentionally non-bootable intermediate revision: worker admission fails
until Stage 2 supplies daemon-owned Changes, and Stage 3 must supply bounded
build storage and immutable executable bundles.

In particular, do not:

- run `factoryctl init` or `factoryctl update --install` from this revision;
- replace binaries under `~/.dark-factory/bin`;
- load or restart the installed launchd job;
- migrate the operator database to schema 30; or
- use the operator home or socket for a test.

Developers use a temporary `DARK_FACTORY_HOME` and explicit socket as described
in the [development workflow](development/WORKFLOW.md). The prior release may
remain installed but must stay stopped as directed by the operator.

Installation documentation will be restored only after all three stages, the
causal proof matrix, hosted gates, and an independent boot review pass. Passing
that gate will not itself install, release, start, or modify the operator
system; each is a separate explicit decision.
