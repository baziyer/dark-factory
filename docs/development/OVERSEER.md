# Overseer runbook

An overseer is an agent with `--role orchestrator`. Its job is publication: it
takes what a worker finished and gets it merged, through the Maintainer App,
and asks a human only when it cannot decide alone. It never writes code.

The overseer's standing instruction (CONFIG → RULES → WHEN IDLE → run a
standing instruction) is short, because a Claude launch delivers the task
through the terminal and that prepared prompt is capped at 8 KiB:

> You are the overseer of the project named PROJECT. Run
> `git clone --filter=blob:none https://github.com/OWNER/REPO repo`, read
> repo/docs/development/OVERSEER.md, and follow it exactly. Before exiting,
> report the durable outcome with `$DARK_FACTORY_FACTORYCTL attempt succeed
> --result` (one line per change you handled, or "nothing to publish").

Every command below runs from the directory the session starts in, its
private runtime home, with the clone at `repo` inside it.

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
       p.name AS project, p.root, t.title, t.body,
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
`$home/changes/<change_id>`. A change is finished when its `enqueue`
operation (step 5) is `completed` in the App journal and the merge was
observed; anything short of that is resumed at the first step whose
operation is not completed, as section 2 says, and a change whose pull
request exists but was blocked waits for a new retained change (section 5
says how to tell).

## 2. Derive one operation id per step, and check the journal first

Every App write takes an `operation_id`. Derive it from the change and the
step so a retry is a replay, never a second publication:

```sh
opid() { python3 -c "import sys,uuid; print(uuid.uuid5(uuid.NAMESPACE_URL, 'dark-factory:' + sys.argv[1] + ':' + sys.argv[2]))" "$1" "$2"; }
# opid CHANGE_ID STEP   with STEP one of: issue, publish-1, publish-2, ..., pr, review-HEAD8, enqueue
```

`review-HEAD8` takes the first eight hex digits of the pull request head it
reviews, so a fix pushed later gets a review of its own.

One id per App write: a change that needs several commits (step 3) uses
`publish-1`, `publish-2` and so on, one per commit, and the same request under
the same id is a replay while a different request under it is refused. Before
each write call `observe_operation` with its id. `completed` means that write
already happened: take its result (for a commit, the head it returned) and go
on to the next step, which for a multi-commit publication is the next commit,
not the pull request. Never received or `planned` means it has not happened.
`executing` or `indeterminate` means stop and raise a human request with the
id.

## 3. Publish the change as a branch

Compute the diff against the base commit without a checkout: the clone's
object store, its index filled from the base commit, and the retained tree as
the work tree. `git add -A` respects the tree's own `.gitignore`, so build
output the worker left behind is not published.

```sh
export GIT_DIR=$PWD/repo/.git GIT_WORK_TREE=$home/changes/$change_id GIT_INDEX_FILE=$PWD/change.index
git fetch -q origin "$base_commit"
git read-tree "$base_commit" && git add -A
git diff --cached --name-status "$base_commit"   # A / M / D per path
git diff --cached --numstat "$base_commit"       # for the delta paragraph
git ls-files --stage                             # mode and blob per path
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
```

Build the `changes` array for `publish_commit`: added and modified paths carry
`content_base64` and the `mode` the staged entry shows (`100644` or
`100755`); deleted paths carry only `path`. The App takes at most 50 entries
per commit and 1,000,000 base64 characters per file (about 732 KiB of
content), and refuses `.github/workflows`, the CODEOWNERS locations and the
dependabot config. More than 50 files means several commits on the same
branch, each bound to the head the previous one returned. A file over that
bound, a symlink (staged mode `120000`), or a refused path is a human request,
not a workaround.

Then, with `branch = factory/<first 12 hex of change_id>`:

1. `observe_ref` for `main` and keep the answer as `main_head`. If it is not
   `base_commit`, main moved since the worker started; publish anyway from
   `base_commit` and let the queue merge it, but say so in the body.
2. `publish_commit` with `operation_id = opid publish-1`, `branch`,
   `expected_head_sha = base_commit`, a one-line message from the task title,
   and the first (or only) 50 entries. It returns the new head commit; a
   second commit uses `opid publish-2` and that head, and so on. The last
   returned head is the pull request head.

## 4. Open the issue and the pull request

`create_pull_request` needs an issue. `create_issue` with `opid issue`, the
task title (cut to 256 characters, the App's bound), and a body of the task
text plus the change id; it returns the issue number. Then read
`observe_ref` for `main` again, immediately before the call, and use that
answer: `create_pull_request` with `opid pr`, `issue_number` from that
result, `head = branch`, `head_sha` = the last published commit, `base =
main`, `base_sha` = main's head as just read (the App verifies the base
branch is at that commit at that moment; `base_commit` is wrong whenever
main moved, and a stale read is wrong whenever main moves between the read
and the call), `draft = false`, the same title, and a body in this
repository's shape. An operation
id belongs to the exact request it was first sent with, so a request the App
refuses for its content cannot be corrected under the same id: that is a
human request, not a retry.

- What changed and why: from the task and the diff, in prose.
- Production-line delta: added minus deleted outside tests, docs and fixtures,
  from the numstat, with the largest files named.
- How it was verified: what the worker's result text says it ran, and that
  the merge queue runs `scripts/local-ci.sh`. Claim nothing you did not see.
- Never an email, an org name, an account id, or a `/Users/<name>` path.

Write that body to a file; the review needs it.

## 5. Get the cold review, then merge

```sh
DARK_FACTORY_REVIEW_OPERATION_ID=$(opid "$change_id" "review-$(printf '%s' "$HEAD_SHA" | cut -c1-8)") \
    repo/scripts/cold-review.sh OWNER/REPO PR HEAD_SHA BASE_SHA body.md "first review"
```

The verdict is recorded under that operation id, so `observe_operation`
with it answers `completed` with the verdict (`allow` or `block`) once a
review exists, and nothing until then. The script exits 0 for ALLOW, 1 for
REQUEST_CHANGES, 3 when the session reported no verdict, 4 when the pull
request is no longer at that head, and 2 for an argument it refuses, and
leaves `review-PR-HEAD8.log` in the current directory.

- Exit 3: run it once more; a second 3 is a human request with the log's
  last lines.
- ALLOW: `enqueue_pull_request` with `opid enqueue`, the PR number, the head
  and `base = main`. Then `observe_pull_request_merge`, with the PR number,
  the head, `base = main` and the enqueue operation id, every 60 s for up to
  30 minutes;
  never faster. Merged: done. Still queued after 30 minutes: the change is
  not finished; raise a human request naming the PR and stop, and the next
  run observes it again. No longer queued and not merged:
  the queue's run failed or dropped the entry, and the App cannot read a
  queue run's log or rerun it (`read_pull_request_job_log` and
  `rerun_failed_pull_request_jobs` bind to the pull request's own runs), so
  raise a human request with the PR link; never enqueue again on your own.
  An ALLOW the App did not record shows up the same way: the queue's review
  check refuses the entry.
- REQUEST_CHANGES: you do not fix code. Raise a human request with the PR
  link and the findings verbatim; the human enqueues the fix as a task to the
  worker. Stop handling this change until a new retained change for the same
  task appears.
- Exit 4: the pull request is no longer at the head you published, which
  only a person can have done; raise a human request.
- Exit 2 or 5: the script refused its arguments or could not prepare the
  checkout; the log was not written. Check the head and base you passed once,
  then raise a human request with the script's message.

On a resumed run, a change whose `pr` is completed but whose `enqueue` is not
needs no second review if one was recorded: `observe_operation` with `opid
review-HEAD8` for the pull request head answers `completed` with verdict
`allow` (enqueue), `block` (blocked: wait for a new retained change), `note`
(a comment that decided nothing: run the review again under a fresh id, the
head's plus `-2`), or nothing (run the review); `executing` or
`indeterminate` is a human request, as in section 2. The `review` check
itself runs only in the merge queue, so it is never the signal here.

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
