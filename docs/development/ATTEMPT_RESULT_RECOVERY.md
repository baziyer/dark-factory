# AttemptResult composition and finite-recovery spec

Working spec for the daemon composition phase on `attempt-result-integration`
(kernel checkpoint ALLOW `026350a7` merged with runner checkpoint ALLOW
`c102872d`). Source of truth for this phase until the composition is built;
retire or mark residual sections when the work lands. Derived from the
corrected AttemptResult contract, the supervisor ordering (`StartBlocked` →
Store `ActivateRunner` → `child.Activate()` marker → exec release), and the
runner publish/authenticate protocol.

## Ground rules

- The Store cannot see markers. Marker/artifact/lease residue selects the
  recovery cell; the kernel transaction is chosen by Store footprint plus
  caller-proved evidence.
- Absence is only ever a positive exact-identity proof (pid+pgid+birth
  digest) plus runtime-lifetime-lease availability. A numeric PID is never
  authority.
- Lease availability semantics: flock acquirable ⇔ no live holder in the
  attempt tree. VERIFIED in composition: the gate clears CLOEXEC on the
  inherited lifetime descriptor before exec (process_darwin.go, gate
  retain step), and the inner gate repeats the same retention, so the
  outer runner AND the provider each hold the one lease OFD. Arbitrary
  provider descendants are NOT guaranteed holders, so the sweep never
  concludes absence from lease availability alone: every absence is a
  positive per-identity observation on top of the acquired lease.
- Failure details must state only what was observed (declarations-vs-
  observations rule).
- A present-but-non-authenticating artifact is evidence of a torn publish
  (O_EXCL at final name, no rename step). Never unlink it on a guess; see
  cell C3.

## Invariants that make cells impossible (fail closed if observed)

- Outer marker with Store runner=`starting`: impossible (marker is created
  only after `ActivateRunner` commits). Treat as corrupt/hostile;
  unresolved + alert.
- Any authenticated artifact without the outer marker: impossible (writer
  is the outer target, which execs only after the durable marker). Reject.
- `inner_unregistered_converged` artifact with inner marker present:
  rejected by writer, reader, and revalidation (pinned by
  TestAttemptResultMarkerMatrixIsClosed).
- Census legality constraints in `inspectRecoveredRuntimeCensus` stay
  as-is; the census grammar must be EXTENDED to admit
  `attempt-result.json` (today `validRecoveredRuntimeFile` returns
  invalid-contract for it — a blocking gap for this phase).

## Recovery cells (run non-terminal, daemon restarted)

Key: R=runner resource state, P=provider pair state, O/I=outer/inner
marker, A=artifact, L=lease available.

- **A. R=starting, P=declared, no O, no I, no A, L** →
  `RecordUnregisteredRunnerConverged`. Covers cuts from BeginRunnerStart
  through pre-ActivateRunner death, including a blocked gate child that
  died un-exec'd.
- **B. R=active(bound), P=declared, no O, no A, L, positive runner
  absence** → `RecordRecoveredPreSessionRunnerAbsence`. Cut: ActivateRunner
  committed, daemon died before marker creation; the orphaned blocked
  child exits when the activation pipe closes.
- **C1. R=active, P=declared, O present, I absent, no A, L, positive
  absence** → same edge as B (the Store footprint is identical; markers
  are invisible to the Store).
- **C2. As C1 but A present and authenticates (necessarily
  inner_unregistered_converged)** → `ConsumeAttemptResult` (admitted-phase
  unregistered arm) → `RecordRecoveredRunnerAbsence` →
  `CloseTerminalAfterRunner` → `AuthorizeAttemptResultRemoval` + unlink →
  runtime release → terminal commit.
- **C3. A present but does NOT authenticate** → torn publish or tampering.
  Do not consume, do not unlink, do not treat as absence-of-result for a
  happy edge. Default fail-closed: unresolved-with-reason, retain the
  file.
- **D. R=active, P=active (identity bound), no A, L, positive absence of
  BOTH identities** → no trusted result exists; phase may be admitted or
  running. Failure proposal to finalizing (`FailRun`), provider pair
  `MarkProviderResourcesUnresolved` (release of a bound pair requires
  ProviderExit, which does not exist here), runner via
  `RecordRecoveredRunnerAbsence`, session via the recovered close edge.
- **E. A present and authenticates as inner_converged** (P declared or
  active, phase admitted/running/finalizing) → `ConsumeAttemptResult`
  (releases the provider pair in every arm) → `RecordRecoveredRunnerAbsence`
  (a restarted daemon cannot Wait a non-child) → `CloseTerminalAfterRunner`
  → `AuthorizeAttemptResultRemoval` + unlink → runtime release → terminal
  commit.
- **F. L NOT available (live holder)** → the attempt tree is alive; the
  daemon must not conclude anything. Bounded observation: watch for
  artifact appearance/lease release; never signal on recovered numeric
  identity; the notice socket is gone, so recovery consumes by polling
  authentication of the spool. Finite = bounded poll interval + no
  invented absence, not a timeout that fabricates failure.

**Sweep ordering rule (binding):** always attempt result authentication
BEFORE any absence edge. Firing `RecordRecoveredPreSessionRunnerAbsence`
while a result artifact exists finalizes FailureActivation and then
`AuthorizeAttemptResultRemoval` refuses forever (orphan file, no false
exit) — fail-closed but stuck; the ordering rule prevents entering that
state.

## Composition invariants (from the checkpoint reviews)

1. **O1 (binding):** runner `AuthenticateAttemptResult` does NOT verify
   the proof — a forged artifact with a wrong proof authenticates and
   merely reports a different `ProofDigest()`. The daemon must make the
   comparison of `record.ProofDigest()` against the kernel run's stored
   result-proof digest impossible to skip: the only constructor turning a
   runner record into the kernel `AttemptResult` binds the digest from the
   record, and the Store performs the equality itself. Causal test: a
   forged-proof artifact is refused by the composed flow.
2. **O3:** `inner_converged` durable evidence carries only code/signal;
   abort context is derived from protocol state, not the artifact.
3. **O4:** a publish that fails after the O_EXCL create can leave residue
   removable only by whole-directory teardown; the census-flip case leaves
   a FULLY VALID artifact behind. Cell C3 accounts for both.
4. **O7 (threat model, document not fix):** the artifact carries the raw
   proof secret in a same-uid 0600 file; a surviving same-uid descendant
   can read it post-publication and, in the notice-less recovery path,
   unlink+forge a differently-shaped authenticating result. The proof
   defends the artifact channel, not the same-uid boundary; recovery must
   not claim otherwise.

## Built in this phase (status)

- Live composition (supervisor): BeginRunnerStart/ActivateRunner windowed
  failure convergence, ACK-free consume tail, O1 single-constructor proof
  binding with a composed causal test, disk-authentication fallback when a
  late credit/terminate write poisons the control socket (writeFrame
  closes the socket on any failed write — the notice was never authority).
- Kernel fix surfaced by composition (own flagged commit, needs
  re-review): terminalResultPostcondition and the consume replay
  recognizer compared insignificant exit-arm values, wedging every signal
  and nonzero-code exit at the replay/close/removal seams; fixed with
  significance-aware comparison and a causal test over both arms.
- Recovery sweep: RecoverAbandonedRuns implements cells A, B/C1, C2/E, C3
  (retain, conclude nothing), D (fail closed, unresolved pair, absence-
  released runner, deliberately not terminal), F (held lease concludes
  nothing). Census admits attempt-result.json with outer-marker + token
  provenance; recovered runtime exposes marker facts, notice-less
  authentication, and exact removal.
- Spec corrections from building it: cell D needs no new session-close
  edge — the run deliberately remains finalizing with unresolved residue
  (finalizeRun refuses unresolved footprints), which is the intended
  operator surface; worker-run terminal commit in recovery requires the
  change settlement (published-tree inspection) and is deferred to the
  scheduler/factoryd phase; census legality additionally requires an
  artifact to imply outer marker AND token, and an outer marker without
  the worker config still implies the inner marker (production publishes
  config before start — recovery fixtures must too).

## Migration debt closed by this phase

- internal/daemon compile failures (removed AttemptEvent.Terminal /
  AcknowledgeTerminal / AttemptSpec.TerminalName) and ~63 test failures
  (fixtures using the removed generic runner activation and
  ObserveRunnerExit) migrate to the exact edges: BeginRunnerStart →
  ActivateRunner → ActivateProviderResources; failures via FailRun /
  FailRunWithRuntimeAbsent; runner exits via RecordLiveRunnerExitAndRelease
  / RecordRecoveredRunnerAbsence; the unregistered/pre-session edges where
  the old paths conflated them.
- After migration, per review conditions: delete the ObserveRunnerExit
  stub and the dead runner arm of observeProcessExit, the now-unproducible
  FailureRunnerExit code, and the legacy terminal-close methods including
  resultDerivedTerminalClose (same-millisecond fingerprint wedge).
- Checkpoint ALLOW conditions resolved here: `go build ./...` restored;
  full `go test ./...` green.
- Done in the simplicity audit (ff0ca005): the legacy terminal-close
  methods and resultDerivedTerminalClose are deleted; kernel test
  fixtures now close sessions by reconstructing the run's exact
  AttemptResult through CloseTerminalAfterRunner, and no-result histories
  are pinned as deliberately non-terminal.

## Built in the activation phase (factoryd)

- The sweep is wired into factoryd boot after the daemon opens and before
  any listener exists; every disposition is reported, and unresolved
  residue never refuses boot.
- Settlement is composed from the reviewed FinalizeRun/FinalizeWorkerRun
  edges at the scheduler completion seam and the sweep's converging arms:
  an unpublished candidate change settles abandoned; a published change
  settles retained after the published tree is re-read and verified
  against its recorded identity, base and format (finalName is the change
  ID, format/base/stage come from the stored selection and tree identity);
  the tree's contents settle as the worker left them. The black-box
  daemon E2E proves the consumed-on-boot crash cut ends at a terminal
  failed task with its published change retained, before the socket opens.
- A settlement refusal is surfaced, never fatal and never silent: the
  sweep reports `result-consumed-unsettled` as its own disposition, and a
  live scheduled completion that cannot settle reports the run through
  the SupervisorSpec's UnsettledCompletion while the scheduler keeps
  serving every other attempt.
- The honest limit, proven live: the runner is the sole exit-observation
  authority for its provider, so a runner death mid-attempt leaves
  releasing residue no edge can settle — the run stays deliberately
  nonterminal (live cell D), its task visibly running, and it consumes a
  capacity slot and blocks its agent until an operator resolution surface
  exists. At the default capacity of one this idles the factory; that is
  honest accounting, not a fabricated outcome, and the black-box E2E pins
  the whole shape (daemon survives, wedge surfaced, capacity accounted).
