# Architecture review — 9 September 2026

The current ownership boundaries are worth keeping. The kernel owns durable state and admission; the daemon coordinates external effects; the runner owns provider processes and PTYs; the CLI and browser are clients. There is no internal package dependency cycle. The largest opportunities are repeated representations and initialization, rather than adding a new service layer.

This review traced task submission/editing, supervision, terminal controls and receipts, floor projection, local API authentication, and schema migration. It also inspected the production package dependency graph and searched for identical code blocks. It is not a line-by-line audit of every process/recovery path or a claim that all duplication is gone.

## Changes supported by the review

- Reuse the existing daemon constructor for public construction and the injected-clock test seam. Both now initialize exactly the same maps, gates and scheduler channel.
- Keep one socket-identity check in the shared API client. Darwin and Linux retain only their distinct peer-credential checks.
- Promote the existing identifier byte-copy method rather than repeating a forwarding method for every typed ID. Keep distinct ID types, validation, defensive copying and existing text-marshalling behavior.
- Derive sprite activity from the operator-facing status calculation instead of maintaining two copies of question/activity/pause precedence.
- Extend queued task update and private task detail for brief editing and conversation history. Do not create a second queue or expose private text in the public snapshot.
- Use one peer question with an optional answer and separate notification receipts. Derive recipient availability from task state rather than persist another overall workflow state machine.
- Build historical schema sets from the next version, with exact digest tests, rather than repeat exclusions for new objects in every older version.

## Opportunities to keep in view

| Area | Evidence and consequence | Smallest useful next step |
| --- | --- | --- |
| UI controller | Connection generations, terminal ownership, configuration, task actions and topology polling live together in `factory-app-controller.ts`. Splitting it arbitrarily would move complexity and make lifetime ordering harder to see. | Keep pure selectors and layout outside the controller; extract another owner only when it has an independent lifetime and more than one caller. |
| Protocol changes | One operation crosses Go wire shapes, domain mapping, TypeScript decoding and UI behavior. A missed projection can make a backend capability inaccessible. | Add related features through an existing operation/detail shape where semantics match, and leave one real boundary test for every new operation. Avoid an additional forwarding layer or a generator without measured maintenance benefit. |
| Status vocabulary | Durable task/run states, operator labels and sprite activity are different projections. Duplicated precedence rules can disagree. | Centralize each projection and derive presentation from it; keep durable task outcome distinct from run cleanup phase. |
| Delivery semantics | A PTY write may succeed while its reply is lost. Retry wrappers can duplicate a message or answer. | Reuse the serialized effect path and explicit receipt states. Keep “saved”, “delivered”, and “unknown” distinct. Never replay unknown delivery automatically. |
| Historical schemas | Current schema edits previously changed the reconstructed identity of older homes. | Freeze replaced SQL and derive versions in order; require historical digest and preservation checks for each migration. |
| Reconciliation | Full invariant validation runs on store writes. A short deadline can interrupt valid recovery, while replaying a committed result with a newly read revision can reject its own postcondition. | Use a measured bounded store window and exact durable replay postconditions; check interruption between recovery stages. Profile validation cost as retained history grows before changing its authority boundary. |

## Checks that should remain

Input grammar validation, transactional authorization and filesystem/process identity checks protect different boundaries. Similar-looking checks are not automatically redundant. Keep revision fences at mutations, schema/integrity validation before admission, exact ownership before cleanup, and independent protocol validation at each endpoint. UI preflight may improve feedback, but it must not become a second authority or an unnecessary prerequisite for an already valid live control.

Do not merge human decisions, peer questions and operator interventions into one generic message workflow: they have different permitted actors, recipients and effects. Share the actual delivery machinery where those semantics coincide.

The shared backend cleanup removes 30 production lines. The new editing and peer capabilities add code because they cross existing authority and wire boundaries; they are not a net-negative refactor. The pull request records the cumulative production delta and the exact validation receipt.

The review also caught concrete integration errors: inaccessible older peer messages, missing peer discovery, notification retry conflicts, and presentation that could mistake an old run sample for current work. Each needs a causal check at the boundary where it failed, rather than another layer of generic validation.
