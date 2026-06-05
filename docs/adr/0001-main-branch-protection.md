# ADR-0001: Main branch protection + release.yml portability retrospective

## Status

Accepted — 2026-06-05. Implemented by PR #67 (release.yml `paths:` filter
removal) and the live `gh api PUT /repos/TinyWhite1997/LazyAgents/branches/main/protection`
flip executed immediately after PR #67 squash-merged at commit
`5d76b46`.

Supersedes: none. Related: ADR-0002 (cron DST fall-back take-first).

Drafted by Backend Architect at Software Architect's request after the
five-hotfix `v0.1.0-rc.1` chain (PR #62 / #63 / #64 / #65 / #66) finally
landed the release end-to-end.

## Context

WEK-78 / M4.6 is the M4 milestone closer. Its DoD requires:

1. R2 — `release.yml` tag-driven pipeline that actually publishes a
   GitHub Release for `v0.x.0-rc.1`.
2. R1 / A1 — main branch protection flipped on with a fixed list of
   required status checks, **no advisory window**.
3. A7 — every follow-up issue must be filed **before** M4.6 done.
4. Five known-red / backfill items must each be addressed.

This ADR records two related decisions:

- Which exact required-check contexts to pin on `main`, and why the
  literal strings are not negotiable.
- How to handle workflow-level `paths:` filters once those checks
  become required (chose option A: drop the filter; rejected option B:
  configure protection in a "required only when present" shape).

It also captures the portability lessons surfaced by the five `rc.1`
tag re-cuts that were needed before `release.yml` published
end-to-end, so future readers can avoid the same root causes when
changing release infrastructure.

## Decision

### 1. Required status checks on `main`

Branch protection on `main` requires the following six status check
contexts. They are stored verbatim in
`gh api /repos/TinyWhite1997/LazyAgents/branches/main/protection`'s
`required_status_checks.contexts` and must remain byte-identical to
the workflow `name:` fields they come from.

```
ci / ubuntu-22.04
ci / macos-14
ci / macos-15-intel
ci / windows-2022
pr validate (cargo dist plan + actionlint)
pr build dry-run (x86_64-linux-gnu + musl)
```

Derivation:

- `ci / <os>` comes from `.github/workflows/ci.yml`'s matrix job whose
  `name:` is `ci / ${{ matrix.os }}`. The four runner labels in the
  matrix are A5.3 hard-pins — `*-latest` is banned because tag-cut
  reproducibility depends on the image staying stable. When GitHub
  retires a pinned image, the recovery rule is "pin the next fixed
  image, never revert to `*-latest`": macOS Intel migrated from
  `macos-13` to `macos-15-intel` in PR #60 after the 2025-12-04
  macOS 13 image retirement (see release.yml:340-353 for the inline
  rationale and the macOS reviewer's authorization).
- `pr validate (cargo dist plan + actionlint)` and
  `pr build dry-run (x86_64-linux-gnu + musl)` are the full `name:`
  fields of the two PR-eligible jobs in `release.yml` (jobs
  `pr_validate` and `pr_build_dryrun`). The trailing parenthetical IS
  part of the check context — using the YAML job key instead would
  silently fail to match.

Three literal-format invariants:

| Invariant | Why it matters | Failure mode if violated |
|---|---|---|
| `ci / <os>` — exactly one space on each side of the slash | The string IS the check context; ` / ` (space-slash-space) is the rendered name | GitHub displays no error; PR sits with `Some checks haven't completed yet` until manual admin override |
| Full `name:` field for `pr_*` jobs, including parentheticals | The rendered check context uses `name:`, not the job key | Same silent hang |
| `app_id` for all 6 must be GitHub Actions (15368) | Branch protection records the producing app alongside the context | A third-party app reporting a same-named context would not satisfy the rule |

The live `app_id: 15368` for all six contexts in the merged protection
response confirms they're all GitHub Actions-emitted, not a typo'd
duplicate from a different integration.

### 2. `paths:` filter on PR-eligible release.yml jobs — option A (drop)

`release.yml` originally had a `paths:` clause on the `pull_request`
trigger restricting `pr_validate` + `pr_build_dryrun` to PRs touching
`Cargo.toml` / `.github/workflows/release.yml` /
`.github/workflows/ci.yml` / `crates/la-daemon/templates/**`. With
branch protection making those two checks required, an unfiltered PR
that didn't touch any of those paths would hang forever waiting for a
check that GitHub never scheduled.

Two options were considered:

- **A. Remove the `paths:` filter.** Every PR runs both jobs. Cost:
  `pr_validate` adds ~30 s (`cargo dist plan` + actionlint + yamllint);
  `pr_build_dryrun` adds ~10 min (`cargo dist build` for x86_64-gnu +
  musl on ubuntu-22.04). Both run on `runs-on: ubuntu-22.04` only —
  the tag-driven `build` / `global` / `attest` / `notes` / `host` jobs
  stay gated by `needs:` + `if: startsWith(github.ref, 'refs/tags/v')`
  and never trigger on PR.
- **B. Keep `paths:`, configure protection as "required only when
  present".** GitHub does support contexts that don't block when not
  reported, but the UI exposure of that semantics is opaque (no
  explicit toggle named "skip if check absent"; it's a property of
  per-context rules in the newer Rulesets model, not the legacy branch
  protection record). Reviewers would have to inspect the protection
  payload to understand why a PR is or isn't blocked.

Choice: **A**. Reasons:

- A is a single boolean change to `release.yml` (delete the `paths:`
  block, add an explanatory comment), no protection-side complexity.
- A produces uniform behavior for all future PRs — the same six checks
  always run, always block, no path-dependent surprises.
- A's PR cost is bounded and observable: the slowest job is
  `pr_build_dryrun` at ~10 min, well inside the ≤25 min PR CI budget
  per the M4 brief.
- B's correctness depends on a quiet GitHub API contract that has
  shifted before (legacy branch protection vs. rulesets) and gives
  reviewers no in-UI signal.

Implementation: PR #67 (merged `5d76b46`, 2026-06-05T23:05:41Z) deleted
the `paths:` block from `release.yml:20-30` and replaced it with the
inline comment block now visible at release.yml:20-30 explaining the
WEK-78 reason for unconditional firing.

### 3. `gh api PUT /repos/.../branches/main/protection` payload

Live response, captured directly after the flip
(`gh api /repos/TinyWhite1997/LazyAgents/branches/main/protection`):

```jsonc
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "ci / ubuntu-22.04",
      "ci / macos-14",
      "ci / macos-15-intel",
      "ci / windows-2022",
      "pr validate (cargo dist plan + actionlint)",
      "pr build dry-run (x86_64-linux-gnu + musl)"
    ],
    "checks": [
      {"context": "ci / ubuntu-22.04",                          "app_id": 15368},
      {"context": "ci / macos-14",                              "app_id": 15368},
      {"context": "ci / macos-15-intel",                        "app_id": 15368},
      {"context": "ci / windows-2022",                          "app_id": 15368},
      {"context": "pr validate (cargo dist plan + actionlint)", "app_id": 15368},
      {"context": "pr build dry-run (x86_64-linux-gnu + musl)", "app_id": 15368}
    ]
  },
  "required_signatures":              {"enabled": false},
  "enforce_admins":                   {"enabled": false},
  "required_linear_history":          {"enabled": false},
  "allow_force_pushes":               {"enabled": false},
  "allow_deletions":                  {"enabled": false},
  "block_creations":                  {"enabled": false},
  "required_conversation_resolution": {"enabled": false},
  "lock_branch":                      {"enabled": false},
  "allow_fork_syncing":               {"enabled": false}
}
```

Per-field rationale:

| Field | Value | Why |
|---|---|---|
| `required_status_checks.strict` | `true` | Forces PR to be rebased / merge-current with `main` before the checks count. Catches "checks were green an hour ago, but main moved on" drift. Trade-off: contributors must rebase before merge; acceptable cost given the 25-min PR CI budget. |
| `required_status_checks.contexts` / `checks` | the 6 contexts above, all `app_id: 15368` | See section 1. The dual `contexts` + `checks` representation is the GitHub API's bridging shape: `contexts` is the legacy flat list, `checks` is the newer per-app form. Branch protection writes both; ruleset migrations down the line read whichever they support. |
| `required_pull_request_reviews` | **absent** | M4.6 brief does not require a human reviewer to gate merge; the six green checks plus reviewer comment threads (Code Reviewer + macOS Code Reviewer) are the only review surface. Revisit at v1.x if CODEOWNERS is introduced. |
| `enforce_admins` | `false` | Admins can override during incident response (broken check, GitHub outage, security hotfix). Tighten to `true` if we ever need a SOC2-style audit. |
| `required_linear_history` | `false` | We squash-merge by convention but don't enforce it at the protection layer — leaves room for `git revert` chains during incident response. |
| `allow_force_pushes` | `false` | Belt-and-braces against `git push --force-with-lease origin main` from anyone, admins included. The five `v0.1.0-rc.1` tag re-force-moves during M4.6 went onto the **tag**, never `main`, precisely because of this guard. |
| `allow_deletions` | `false` | Same reason. |
| `block_creations` | `false` | Doesn't apply to branches; this controls tag/branch creation in protected paths, irrelevant to a `main` rule. |
| `required_conversation_resolution` | `false` | Reviewer comments are not auto-blocking; if we want stricter merge gating we'd flip this and pair with `required_pull_request_reviews`. |
| `lock_branch` | `false` | Branch is not archived. |
| `allow_fork_syncing` | `false` | Default safe; no current fork sync workflow. |

Exact `gh api PUT` invocation that produced this state, for future
replays:

```sh
gh api -X PUT /repos/TinyWhite1997/LazyAgents/branches/main/protection \
  -F 'required_status_checks[strict]=true' \
  -f 'required_status_checks[contexts][]=ci / ubuntu-22.04' \
  -f 'required_status_checks[contexts][]=ci / macos-14' \
  -f 'required_status_checks[contexts][]=ci / macos-15-intel' \
  -f 'required_status_checks[contexts][]=ci / windows-2022' \
  -f 'required_status_checks[contexts][]=pr validate (cargo dist plan + actionlint)' \
  -f 'required_status_checks[contexts][]=pr build dry-run (x86_64-linux-gnu + musl)' \
  -F 'enforce_admins=false' \
  -F 'required_pull_request_reviews=' \
  -F 'restrictions='
```

### 4. Future-change protocol

Each kind of change to the protection contract has a one-PR minimal
shape. None of these are optional — skipping a step here is what
silently hangs PRs.

#### Adding a new required status check (e.g. a new CI OS leg)

1. Land the new job in `ci.yml` / `release.yml` first. Its `name:` is
   the future check context — choose carefully (see invariant 1 above).
2. Wait until that job has been green on **at least one merged PR** so
   the context exists in GitHub's known-checks for the repo.
3. Open a second PR with `gh api -X PUT .../branches/main/protection`
   payload extended by the new context. The PR's diff is in this ADR
   (a new row in section 1's table); add the same string to the
   `required_status_checks.contexts` + `checks` arrays.
4. Update this ADR's section 1 table and the live-response snapshot in
   section 3 in the same PR.

#### Removing a required check (e.g. dropping a deprecated OS leg)

1. Flip protection first via `gh api -X PUT` with the shortened list.
2. Then delete the corresponding job from the workflow.
3. Order matters: removing the job first leaves the protection rule
   referencing a context that no PR will ever satisfy.

#### Adding a `paths:` filter to a required-check workflow

**Don't.** Section 2's option B was rejected for a reason. If you have
a strong reason to think a job's cost can be amortized only by
path-restricting it, write a follow-up issue with the use case and
revisit this ADR; do not silently land it.

#### Adding `required_pull_request_reviews` (CODEOWNERS / review gates)

Single-PR scope: edit the live payload, update sections 3 and 4 of
this ADR. Pair with a CODEOWNERS file commit so contributors know who
is on the hook.

#### Workflow rename (`name:` change on an existing required job)

GitHub does NOT follow renames. The protection rule continues to
require the old context, which no future job emits, so every PR
silently blocks. Recovery is the same as "remove required check then
add a new one" — flip protection first, then rename. Never rename the
`name:` field of a workflow that appears in section 1 without doing
the protection flip in the same PR.

### 5. Failure modes and diagnosis

| Symptom | Most likely root cause | First diagnostic |
|---|---|---|
| PR sits with `Some checks haven't completed yet`, no jobs spinning | Required check context name does not match any workflow `name:` (typo, lost parenthetical, wrong slash spacing) | `gh api /repos/.../branches/main/protection --jq '.required_status_checks.contexts'` vs `gh api /repos/.../actions/workflows/<id>/runs/<n>/jobs --jq '.jobs[].name'` |
| PR sits with `Expected — Waiting for status to be reported` for a `pr_*` job, the rest green | Someone re-added the `paths:` filter to release.yml and the PR doesn't touch the listed paths | `gh pr checks <n>` shows the missing context; check `release.yml` for a `paths:` block on the `pull_request:` trigger |
| Newly-added required check never reports on PRs | The workflow has `if: github.event_name == 'pull_request'` (correct) but a typo'd condition (e.g. `pull-request`) | actionlint catches most; double-check the literal string |
| `gh api` flip succeeds but contexts don't appear in `protection.contexts` | `gh api` request body shape mismatch — most likely sent `contexts` as a JSON string instead of repeated `-f` flags | Compare the response's `required_status_checks` against the payload in section 3 |
| Admins can merge without checks | `enforce_admins: false` (current value) | Intentional for incident response — see section 3 |
| Force-push to `main` succeeds | `allow_force_pushes: true` or admin override | Should be `false`; check the payload |
| Required check name was renamed in the workflow, PR blocks forever | Workflow rename without simultaneous protection flip | See "Workflow rename" in section 4 |

## release.yml portability retrospective

The `v0.1.0-rc.1` tag was force-moved five times during M4.6 because
each `release.yml` run uncovered a previously latent bug that only
fires on the first end-to-end tag execution. None of the bugs were
caught by PR-level CI because the tag-gated `build` / `global` /
`attest` / `notes` / `host` jobs simply don't run on PRs. This section
is the post-mortem; future readers should consult it before editing
`release.yml`.

### Root causes (in order surfaced)

| # | Hotfix PR | Root cause | Failure mode |
|---|---|---|---|
| 1 | #62 | Workspace `[package] version` was `0.1.0`, tag was `v0.1.0-rc.1`. cargo-dist 0.32 requires the `--tag` SemVer to match the workspace version byte-for-byte. | `plan` job exits 255 with `This workspace doesn't have anything for dist to Release! --tag=v0.1.0 will Announce: ...`. All downstream jobs skip. |
| 2 | #63 (part A) | `release.yml` used `find ... -printf '%p (%s bytes)\n'` in two macOS-reachable steps. `-printf` is a GNU `find` extension; BSD `find` (macOS) rejects with `unknown primary or operator`. | `build (aarch64-apple-darwin)` + `build (x86_64-apple-darwin)` exit 1 in the `extract dist archives for inspection` step. |
| 3 | #63 (part B) | `release.yml`'s `pr_build_dryrun` job had `PR_DRYRUN_TAG: "v0.1.0"` hardcoded. PR #62's bump didn't update this env var. | Every subsequent PR with `paths:` matching had its `pr_build_dryrun` fail the same way as case 1. |
| 4 | #64 | `release.yml` used `mapfile -t bins < <(find ...)` in three sites. `mapfile` is a bash 4+ builtin; macOS hosted runners default `/bin/bash` to 3.2.57 (Apple's last GPLv2 ship). | `measure stripped artifact size` step on both macOS legs exits 127 with `mapfile: command not found`. |
| 5 | #65 (later **withdrawn** as a misdiagnosis) | Author inferred from `dist host --help` that `--steps=create` would create the GitHub Release. Did not verify against cargo-dist sources. | Symptom persisted; the only effect was the comment string in `release.yml`. |
| 6 | #66 (actual fix) | cargo-dist 0.32's `dist host --steps=create/upload/announce/release` subcommands only emit manifest intent into `dist-manifest.json`; the **actual** `gh release create` mutation lives in the `publish_github.yml.j2` partial that hand-curated workflows (with `[workspace.metadata.dist].allow-dirty = ["ci"]`) do NOT include. | `host` job's shell `gh release view "$TAG"` returns `release not found`, exits 1. `gh release list` confirms zero releases on the repo. |

PR #66's fix replaced the entire broken `dist host --steps=*` chain
with a direct `gh release create "$TAG" "${assets[@]}" --notes-file
... --prerelease`, replicating the primitives that
`publish_github.yml.j2` itself uses.

### Cross-cutting lessons

- **First-tag-cut is its own test environment.** Anything in
  `release.yml` that is gated by `needs: [build]` / `if:
  startsWith(github.ref, 'refs/tags/v')` is not exercised by PR CI.
  When adding such a step, either (a) move enough of it into
  `pr_build_dryrun` to cover the failure surface, or (b) be prepared
  for one or more re-tag iterations on the first real cut.
- **macOS hosted runners are not Linux.** Apple ships bash 3.2.57 and
  BSD `find` / `sed` / `xargs`. Any GNU-only shell idiom in a step that
  runs on `macos-*` (matrix or otherwise) is a latent landmine. Quick
  portability mental checklist:
  - `find ... -printf` → `find ... -exec sh -c '...wc -c...' sh {} +`
  - `mapfile -t bins < <(find ...)` → `bins=(); while IFS= read -r l;
    do bins+=("$l"); done < <(find ...)`
  - `sed -i` (GNU in-place) → `sed -i.bak ... && rm *.bak` for BSD
    compatibility, or just use python / awk.
  - `xargs -r` → not portable; use `find ... -print0 | xargs -0` and
    let the empty-input case run through (most commands no-op on empty
    args).
- **`cargo dist init`-generated wiring is not always what
  `dist host --help` documents.** cargo-dist's `--steps=*` subcommand
  flags emit manifest intent, while the actual GitHub mutations live in
  Jinja partials (`templates/ci/github/partials/publish_github.yml.j2`
  etc.) that get inlined into auto-generated workflows. Hand-curated
  workflows (`[workspace.metadata.dist].allow-dirty = ["ci"]`) miss
  those partials by design. When in doubt, `gh api
  /repos/axodotdev/cargo-dist/contents/cargo-dist/templates/...?ref=v0.32.0`
  beats `--help` for ground truth.
- **Hardcoded version strings hide in `.github/workflows/`.** PR #62
  bumped 14 in-workspace `Cargo.toml` versions but missed
  `release.yml`'s `PR_DRYRUN_TAG`. The audit hook now in
  `release.yml:138-145` recommends `git grep -nE
  '0\.[0-9]+\.[0-9]+' .github/workflows/` before any future version
  bump.
- **Diagnose via cargo-dist source before changing flags.** PR #65 was
  built on a CLI-help-only reading of `dist host --steps=create`. The
  actual `create` step writes manifest intent; the GitHub mutation is
  done elsewhere. The author's self-correction (PR #66) was after
  fetching `cargo/templates/ci/github/release.yml.j2` and the
  publish_github partial. **For any third-party tool whose source is
  public, the templates + source code beat the CLI help for
  wiring questions.**

## Alternatives considered

### For section 1 (required check literals)

- Pin a smaller subset (only the four `ci / <os>` legs). Rejected:
  brief explicitly requires the two `pr_*` jobs as required checks,
  and they catch release.yml regressions that ci.yml doesn't (cargo
  dist plan validity, target-count regression, musl plan presence).

### For section 2 (paths-filter handling)

- See option B above. Rejected for opacity.
- Move `pr_validate` + `pr_build_dryrun` from `release.yml` to
  `ci.yml`. Rejected: separation of concerns. release.yml owns its
  PR-level verification; ci.yml owns cross-OS test + lint. Merging
  them creates a 200+ line YAML monolith and complicates ownership
  during incident response.

### For section 3 (protection payload)

- Use GitHub's newer Rulesets API instead of legacy branch protection.
  Deferred: the two APIs are interoperable for our use case; legacy
  is well-documented and stable. Revisit if we add CODEOWNERS-gated
  reviews or stacked PR requirements.
- Enable `enforce_admins=true`. Rejected for incident-response
  flexibility; revisit if/when SOC2-style audit requires it.

## Consequences

Positive:

- The merge bar on `main` is now mechanical: six green CI contexts +
  rebase-current, no human judgment required at the protection layer.
- Future protection changes have a written protocol (section 4); no
  more "what was the right gh api invocation again?" each time.
- The release.yml portability retrospective gives the next author of
  release-pipeline changes a checklist to consult before editing,
  saving the same six PRs of trial-and-error we burned in M4.6.

Negative / accepted trade-offs:

- Every PR pays ~10 min for `pr_build_dryrun`. This is the cost of
  not having a release.yml regression slip into a tag-cut again.
- `enforce_admins=false` leaves a manual override for incident
  response. Acceptable because the admin set is small and the audit
  trail is the `gh api` call history.
- Tag re-force-moves are still allowed during emergency re-cuts
  because `allow_force_pushes` only protects branches, not tags. M4.6
  used this five times; future RC cycles likely will too. Document
  every force-move in the tag's annotated message so the historical
  chain is recoverable.

## References

- WEK-78 (this milestone closer): https://multica/issues/d705eb3b-f427-436a-951f-11bf86627eae
- WEK-63 (M4 epic): https://multica/issues/3f506406-21ac-437b-893b-e46fc34f6ce5
- WEK-76 / M4.3 (cargo-dist 6-target release matrix, where release.yml
  was originally hand-curated): merge `016dc69`
- PR #62 (workspace bump 0.1.0 → 0.1.0-rc.1): merge `9213b92`
- PR #63 (BSD find + PR_DRYRUN_TAG): merge `39ff393`
- PR #64 (bash 3 mapfile): merge `67a3f49`
- PR #65 (withdrawn — dist host --steps=create): merge `0643141`
- PR #66 (actual fix — direct gh release create): merge `c5b72c7`
- PR #67 (drop release.yml `paths:` filter for branch-protection
  prep): merge `5d76b46`
- `gh release` for v0.1.0-rc.1: https://github.com/TinyWhite1997/LazyAgents/releases/tag/v0.1.0-rc.1
- release.yml run 27042494079 (the successful run after PR #66): the
  only run in the rc.1 chain to reach `host (github release)` SUCCESS.
- cargo-dist v0.32.0 source / templates that ground-truthed PR #66:
  `gh api repos/axodotdev/cargo-dist/contents/cargo-dist/templates/ci/github/release.yml.j2?ref=v0.32.0`
  and `partials/publish_github.yml.j2` at the same ref.