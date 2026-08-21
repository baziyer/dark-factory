# Local LaunchAgent frozen

Do not render, install, reload, or remove the Dark Factory LaunchAgent during
the safe-kernel refactor. The installed job and `~/.dark-factory` are the
operator's live system, not development fixtures.

Stage 1 replaces resident sessions with one process per admitted run and adds a
durable resource finalizer. Stage 2 adds daemon-owned plain Changes. Stage 3
and the boot review are still required, so the current source is intentionally
unsupported as a launchd service.

For development, run an isolated source-built daemon only as permitted by
[`docs/development/WORKFLOW.md`](../docs/development/WORKFLOW.md), using a
temporary `DARK_FACTORY_HOME` and explicit private socket. Worker checks use
only the deterministic shell provider and a disposable Git repository.

Do not delete `~/.dark-factory` or preserved source paths. Pre-kernel stop/list
instructions no longer apply because the resident-session model is gone. Any
future service and uninstall procedure must
operate on exact durable run resources, wait for finalization, preserve unique
Changes, and verify the target job identity before mutation.

The normal install, update, rollback, and uninstall guide will return only
after the complete safe-kernel boot review and a separate operator decision to
publish and install a release.
