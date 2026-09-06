# Overseer runbook

An overseer is an agent with `--role orchestrator`. Its job is publication: it
takes what a worker finished and gets it merged, through the Maintainer App,
and asks a human only when it cannot decide alone. It never writes code.

The overseer's standing instruction (CONFIG → RULES → WHEN IDLE → run a
standing instruction) is short, because the daemon caps task text at 8 KiB:

> You are the overseer of the project named PROJECT. Clone
> https://github.com/OWNER/REPO with `git clone --filter=blob:none`, read
> docs/development/OVERSEER.md in that clone, and follow it exactly. Before
> exiting, report the durable outcome with `$DARK_FACTORY_FACTORYCTL attempt
> succeed --result` (one line per change you handled, or "nothing to publish").

Everything below assumes that session: `--dangerously-skip-permissions`, the
operator's own home and login, a private `TMPDIR`, no `gh` credential, git
without any remote credential, and the Maintainer App as the one MCP server
(`maintainer`). Nothing here needs more than that.

## What you may and may not do

- Publish only through the App: `publish_commit`, `create_issue`,
  `create_pull_request`, `enqueue_pull_request`, and the observe tools. Never
  `git push`, never edit the operator's checkout, never write into a retained
  Change.
- Never record a review verdict yourself. The review is a separate headless
  session started by `scripts/cold-review.sh`; you read its verdict.
- One change at a time, in the order the runs finished.
- When a step needs a decision you are not sure of, or a publication is
  blocked twice, raise it with `attempt request-human` and stop. That is the
  NEEDS YOU card on the operator's console.

## 1. Find what a worker finished

The daemon home is two directories above `$DARK_FACTORY_SOCKET`. Its store is
`factory.sqlite3`; read it read-only, never write:

```sh
home=$(dirname "$(dirname "$DARK_FACTORY_SOCKET")")
sqlite3 -readonly -json "file:$home/factory.sqlite3?mode=ro" "
SELECT lower(hex(c.id)) AS change_id, lower(hex(c.base_commit)) AS base_commit,
       c.object_format, p.name AS project, p.root, t.title, t.body,
       r.terminal_result AS result, a.name AS agent
FROM changes c
JOIN runs r ON r.id = c.settled_run_id
JOIN tasks t ON t.id = c.task_id
JOIN agents a ON a.id = r.agent_id
JOIN projects p ON p.id = c.project_id
WHERE c.phase = 'retained' AND r.phase = 'terminal' AND r.terminal_kind = 'succeeded' AND r.role = 'worker'
ORDER BY r.terminal_at_ms"
```

Handle only rows whose `project` is yours. The retained tree of a change is
`$home/changes/<change_id>`. A change you already handled has a completed
`publish` operation in the App journal (step 3); skip it.

## 2. Derive one operation id per step, and check the journal first

Every App write takes an `operation_id`. Derive it from the change so a retry
is a replay, never a second publication:

```sh
opid() { python3 -c "import sys,uuid; print(uuid.uuid5(uuid.NAMESPACE_URL, 'dark-factory:' + sys.argv[1] + ':' + sys.argv[2]))" "$1" "$2"; }
# opid CHANGE_ID issue | publish | pr | enqueue
```

Before each step call `observe_operation` with that id. `completed` means the
step already happened: reuse its result and move on. `executing` or
`indeterminate` means stop and raise a human request with the id.

## 3. Publish the change as a branch

Compute the diff against the base commit without a checkout: the clone's
object store, its index filled from the base commit, and the retained tree as
the work tree. `git add -A` respects the tree's own `.gitignore`, so build
output the worker left behind is not published.

```sh
export GIT_DIR=$PWD/repo/.git GIT_WORK_TREE=$home/changes/$change_id
git fetch -q origin "$base_commit"
git read-tree "$base_commit" && git add -A
git diff --cached --name-status "$base_commit"   # A / M / D per path
git diff --cached --numstat "$base_commit"       # for the delta paragraph
git ls-files --stage                             # mode and blob per path
unset GIT_DIR GIT_WORK_TREE
```

Build the `changes` array for `publish_commit`: added and modified paths carry
`content_base64` and the `mode` the staged entry shows (`100644` or
`100755`); deleted paths carry only `path`. The App takes at most 50 entries
per commit and 1 MB per file, and refuses `.github` and CODEOWNERS paths; more
than 50 files means several commits on the same branch, each bound to the
head the previous one returned. A file over 1 MB or a refused path is a human
request, not a workaround.

Then, with `branch = factory/<first 12 hex of change_id>`:

1. `observe_ref` for `main`. If it is not `base_commit`, main moved since the
   worker started; publish anyway from `base_commit` and let the queue merge
   it, but say so in the body.
2. `publish_commit` with `operation_id = opid publish`, `branch`,
   `expected_head_sha = base_commit`, a one-line message from the task title,
   and the `changes` array. It returns the new head commit.

## 4. Open the issue and the pull request

`create_pull_request` needs an issue. `create_issue` with `opid issue`, the
task title, and a body of the task text plus the change id. Then
`create_pull_request` with `opid pr`, `head = branch`, `head_sha` = the
published commit, `base = main`, `base_sha = base_commit`, `draft = false`,
the task title, and a body in this repository's shape:

- What changed and why: from the task and the diff, in prose.
- Production-line delta: added minus deleted outside tests, docs and fixtures,
  from the numstat, with the largest files named.
- How it was verified: what the worker's result text says it ran, and that
  the merge queue runs `scripts/local-ci.sh`. Claim nothing you did not see.
- Never an email, an org name, an account id, or a `/Users/<name>` path.

Write that body to a file; the review needs it.

## 5. Get the cold review, then merge

From the clone directory:

```sh
scripts/cold-review.sh OWNER/REPO PR HEAD_SHA BASE_SHA body.md "first review"
```

It prints the verdict last and leaves `review-PR-HEAD8.log`.

- ALLOW: `enqueue_pull_request` with `opid enqueue`, the PR number, the head
  and `base = main`. Then `observe_pull_request_merge` every 60 s for up to
  30 minutes. Merged: done. A failed queue run: `read_pull_request_job_log`;
  if the failing test does not touch anything in the diff, rerun once with
  `rerun_failed_pull_request_jobs`; otherwise raise a human request quoting
  the failure. Never poll GitHub faster than once a minute.
- REQUEST_CHANGES: you do not fix code. Raise a human request with the PR
  link and the findings verbatim; the human enqueues the fix as a task to the
  worker. Stop handling this change until a new retained change for the same
  task appears.

## 6. Hand off and finish

If a merged PR touched `cmd/` or `internal/`, the live service needs a
reinstall, and if it touched `web/`, the site needs a re-vendor; raise one
human request naming the merge commit and which of the two applies. Then
report with `attempt succeed --result`, one line per change: change id, PR
number, and merged commit or the reason it stopped.

```sh
"$DARK_FACTORY_FACTORYCTL" attempt request-human --idempotency-key "$(uuidgen | tr -d - | tr A-F a-f)" --question "..."
"$DARK_FACTORY_FACTORYCTL" attempt succeed --result "..."
```

A request-human does not wait for the answer; finish the run after raising
it. The next standing-instruction run picks up where the journal says you
stopped.
