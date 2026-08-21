## What changed and why

<!-- One or two paragraphs. Link the issue(s) this closes. -->

## How it was verified

<!-- Which commands you ran and their outcome. `./scripts/local-ci.sh` is the
     authoritative gate; say so explicitly if any part of it did not run.
     Never imply a check passed that you didn't run. -->

- [ ] `./scripts/local-ci.sh` passed locally
- [ ] Load-bearing paths touched (queue/attempt durability, event projection,
      resource finalization, Change ownership, crash/restart) have a causal test
      that would have caught the bug
- [ ] `README.md` / `ARCHITECTURE.md` / `docs/` updated in this PR, not later
- [ ] The codebase is smaller or simpler than before, not just working
      (dead paths deleted, duplicates collapsed, no speculative abstractions,
      no silent fallbacks)
- [ ] No real `claude`/`codex` prompt was sent unless the change required it;
      no test touches `~/.dark-factory` or the installed launchd job

## Adversarial review (see AGENTS.md, rule 2)

The reviewer is **not** the author. They read the diff cold and try to break
it, then post findings as PR comments — including what they tried and
couldn't break. The author addresses each finding or explains why not; the
reviewer re-checks; only then merge.

Reviewer, tick as you go:

- [ ] **Correctness**: tried to construct an input/state that makes this
      wrong (races, restart mid-operation, empty/oversized inputs, a
      provider/effect that hangs or dies)
- [ ] **Simplification**: looked for a smaller change that does the same
      thing; nothing added that one implementation doesn't need
- [ ] **Security**: nothing widens what an admitted attempt, a webhook caller,
      or an untrusted PR can reach (socket/file modes, hook token, sandbox,
      CI runner)
- [ ] **Docs**: what the docs now say matches what the code now does
- [ ] Findings posted as comments; author responses re-checked
