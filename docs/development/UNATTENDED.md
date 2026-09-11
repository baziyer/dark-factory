# Unattended operation

Start with one explicitly selected repository and project. GitHub is the
backlog; the factory owns execution. Installing the runtime alone does not
authorize importing every issue or unlimited agent use.

## Configuration

Copy `scripts/factory-intake.example.json` to a private operator-owned file.
Choose the repository, project and overseer IDs, a ready label, eligible issue
authors, priorities, and a bounded queue size. Issue text is source material,
not authority to change these settings. The host reads GitHub; agents still
publish exclusively through their Maintainer App. No host credentials are
passed in task instructions.

Use preconfigured Luna/Terra workers and a Sol overseer. Set worker capacity,
a standing supervision instruction, and a finite supervision-run allowance.
The overseer may choose among these workers, pause, reorder, stop, and send
work back; it cannot raise model, account, or project limits through its
scoped controls.

Set project limits before enabling intake:

```sh
factoryctl status
factoryctl project limits --project PROJECT_ID --revision REVISION \
  --run-budget 30 --max-run-seconds 2700
```

The allowance adds this many future admissions to the recorded count; it does
not erase history. Zero disables that ceiling. Duration starts at admission,
includes startup and human waiting, and cancels overdue work through the
normal owned-process cleanup. These are run and wall-clock limits, not token
counts or a monetary spending cap. Legacy tool-budget fields are not provider
usage accounting.

## Standing supervision policy

Combine these rules with the project's acceptance criteria and repository
instructions. Supply product priorities and explicit closure criteria.

- Read all pages of `overseer status`. Preserve direct operator interventions.
  Carry the exact `FACTORY_SOURCE OWNER/REPO#NUMBER` marker into every delegated task,
  so later edits and withdrawals can find all linked work.
- Triage first: verify the problem, reject duplicate or already-fixed work
  with evidence, and prefer deletion or no change when sufficient. Close as
  `not_planned` only within the operator's explicit closure policy. Ambiguous
  product decisions go to Needs You.
- Give workers bounded objectives, acceptance checks, source revisions, and
  allowed files. Prioritise impact and dependencies before arrival order.
  Avoid concurrent edits to the same files. Use the cheapest suitable worker.
- On changed or withdrawn sources, cancel queued work and stop running work
  that no longer applies. Observe those outcomes before delegating a successor.
  Intake is a supervisory event, not an instantaneous GitHub-to-process kill.
- A worker's success is input to review, not issue completion. Inspect its
  tree and receipts, run repository gates and independent exact-head review,
  and send findings back. Never author an ALLOW for your own work.
- Allow at most two repair rounds for the same unresolved failure. Repeated
  publication failures, missing authority, or unclear requirements become one
  human decision. Do not spawn fresh tasks to evade admission limits.
- Reuse the source issue when publishing. Observe stable operation IDs before
  retrying writes. Report merged separately from deployed; acceptance criteria
  that require deployment need a verified host deployment receipt.
- Exit after current supervisory actions. Standing worker events wake the
  next run. Do not consume model turns polling or waiting for another worker.

## Questions

Use `attempt request-human` for actual operator decisions. State what must be
decided, your recommendation and why, and the effect of waiting. Keep longer
investigation detail in the task result.

```sh
factoryctl attempt request-human --idempotency-key HEX32 \
  --question 'This overlaps an existing fix. Close it as a duplicate?' \
  --option 'Close as duplicate with the evidence link' \
  --option 'Keep open for a separate investigation'
```

The first suggestion is recommended, never submitted automatically. The user
can select a suggestion, edit it, or write another answer, then select Answer.
Needs You opens a sole request once; collapsing the row stays respected.
Stop task ends its originating task. Keep the provider attempt alive for its
answer. Ordinary terminal prose is not a human request.

`scripts/factory-notify.py --once --home HOME --receipt RECEIPT` sends a
content-free local macOS notification for new decisions and exhausted project
allowances. macOS notification settings control presentation. The browser is
the answer surface. A new automation failure also produces a content-free
notification on the next scheduled observation; its private health receipt
names the affected component. This does not send email, chat, or GitHub messages.

## Recovery and completion

Run intake once before scheduling it. Its `--status` exposes durable receipts.
GitHub failure is not an empty backlog. Exact tracked issues are reread, and
withdrawals become reconciliation events. Planned enqueues are recorded before
the daemon call; deterministic IDs and read-only database observations recover
lost responses. Only the daemon API writes factory state.

Failed work is not recreated on every poll. Missing evidence and ambiguous
outcomes remain visible. Stop intake to stop importing work; disable dispatch
to stop future admissions. Existing runs require Stop or their duration limit.
Intake never fast-forwards the project root. Every delegated worker uses a
private clean worktree and fetches the current base before making changes.
Before each intake pass, the host pauses dispatch at its exact factory
revision, waits for every active run to finish, then fast-forwards the clean
configured project root from its configured HTTPS origin. It preserves
untracked files and refuses tracked edits, a non-fast-forward, an origin or
branch mismatch, and a five-minute drain timeout. It restores only its own
pause through the same revision guard; an operator change wins. A failed
refresh delays new source intake until the next successful pass.
If a source supervisor reaches its duration limit while a human decision is
unanswered, intake records `needs_operator_recovery` and sends one local alert.
It does not repeat that task. Edit the source issue materially to create a new
supervision event after resolving the decision.

An unattended release is complete only when the configured verification hook
observes the exact merged SHA healthy on the actual target. Never infer this
from worker success, merge, a TCP socket, or an unchanged alias alone.

## Host scheduling and deployment

`scripts/factory-autonomy.py CONFIG --once` runs attention checks and intake;
optional `release_configs` paths run exact-default-head releases and enqueue
one idempotent verified-delivery follow-up for the same project's overseer.
Use `--plist` to generate a launchd StartInterval job. The generated job uses
absolute script/config paths and the host's tool PATH. Install it only after
the one-shot preflight succeeds. Each config gets a separate launchd label. Controllers for the same factory
serialize through a host lock, so their source-refresh and deployment hooks
cannot overlap. Use the controller for scheduled work; direct maintenance
hooks are operator tools.
Each tick writes a mode-0600 `.autonomy.json` health receipt beside the intake
journal, containing only component names and finite status codes.
For private repositories, optionally set `review_mirror_root` to an existing
bare mirror at `ROOT/OWNER/REPOSITORY` whose `origin` is the configured HTTPS
GitHub repository. Create it with `git clone --bare https://github.com/OWNER/REPOSITORY ROOT/OWNER/REPOSITORY` so it retains base history. The host fetches only the base and `refs/pull/N/head` into
that mirror after GitHub reports the exact head. It wakes Sol only for open PRs
whose App footer has `Refs #N` or `Closes #N` for a tracked source issue. The
Sol task receives the mirror and exact head to resume publication; it must not
author its own independent review. No GitHub credential is stored in config or
passed to a task.

A release configuration pins `repository`, `base`, `journal`, `deploy_argv`,
`verify_argv`, `review_verifier`, and `command_timeout` (5–1200 seconds).
All command arrays are trusted operator configuration with absolute executable
paths, never source or agent output. The controller appends the full merge SHA.
The runtime installer can take up to 960 seconds (drain, reinstall, and
probe), so its release configuration uses 1200 seconds.
For this repository use Python with `scripts/deploy-runtime.py` and
`scripts/verify-live-runtime.py`; for the site use `/bin/sh` with
`scripts/deploy-site.sh` and Python with `scripts/verify-live-site.py`.
`review_verifier` runs `/bin/sh` with `scripts/verify-adversarial-review.sh`.
A probe emits the actually installed `sha` and boolean `healthy`; unavailable
or malformed observations block deployment. It must never echo the requested
SHA without observing the target.

`factory-release.py CONFIG --latest --once` requires a merged PR at the
configured default branch, an independent Maintainer ALLOW at its exact head,
and completed passing checks. It records the plan before executing the fixed
hook, then independently probes the live target. A crash or ambiguous effect
is observed before retrying; blocked receipts require explicit `--retry`.
The runtime hook drains active work with dispatch off, uses the stock reinstall
script, checks all three binaries and browser health, and restores dispatch
only if no later operator change superseded its pause. Each hook starts in an
owned process group; a timeout terminates that group before the receipt is
blocked.

Before deployment the release journal records the actually observed live SHA
and maps every merged factory PR in the bounded ancestor range to its one
App-rendered `Refs #N` or `Closes #N` footer. The App operation marker follows
that footer and is accepted. Missing or ambiguous source links, incomplete
range results, and unexpected live tips block deployment. On the first probe,
an already-current live SHA becomes an explicit baseline with no historical
issue sweep; an older observed SHA is mapped before deployment even when it is
unhealthy. A non-ancestor baseline requires explicit
`allow_nonancestor_baseline: true` and deliberately enqueues no delivery work.

For deployment-required source issues, set `close_on_merge: false` when the
App creates the PR. The host's verified-delivery follow-up lets the overseer
close the issue with evidence after deployment. Without an enabled release
configuration, merged work remains a deployment handoff, not automatic delivery.
