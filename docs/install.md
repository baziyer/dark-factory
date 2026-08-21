# Installation frozen

Do not install or update Dark Factory from the safe-kernel refactor. Stages 1
and 2 are merged, and Stage 3 is being implemented on an isolated branch, but
its exact-head gates, independent review, merge, and the separate boot review
remain incomplete.

In particular, do not:

- run `factoryctl init` or `factoryctl update --install` from this revision;
- replace binaries under `~/.dark-factory/bin`;
- load or restart the installed launchd job;
- migrate the operator database to schema 32; or
- use the operator home or socket for a test.

Developers use a temporary `DARK_FACTORY_HOME` and explicit socket as described
in the [development workflow](development/WORKFLOW.md). The prior release may
remain installed but must stay stopped as directed by the operator.

Installation documentation will be restored only after all three stages, the
causal proof matrix, hosted gates, and an independent boot review pass. Passing
that gate will not itself install, release, start, or modify the operator
system; each is a separate explicit decision.
