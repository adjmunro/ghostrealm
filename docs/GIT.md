# Git.md

## Cardinal Rules That Must Never Be Broken:

- COMMITTING AND PUSHING ARE PRE-AUTHORISED: commit and push freely, without asking. This
  project runs autonomously; no per-commit or per-push approval is needed.
- NEVER SIGN COMMITS OR TAGS: always pass `--no-gpg-sign` (`git commit --no-gpg-sign`, likewise for `merge`, `rebase`, `cherry-pick` and `revert`; `git tag --no-sign`). Signing waits for a human to unlock the key, and agents work while no human is present.
- `develop` MOVES ONLY BY FAST-FORWARD, AND ONLY ONTO COMMITS 🔍 Code Reviewer HAS APPROVED (OR THAT ONLY RECORD AN APPROVED REVIEW'S NON-BLOCKING FINDINGS OR NITS, OR THAT WERE REBASED FROM SUCH COMMITS, WITH A DELTA REVIEW WHERE THE REBASE RULE IN BRANCH HYGIENE REQUIRES ONE, AND THEN PASSED THE POST-REBASE BUILD AND TESTS OR WERE EXEMPT). Push it after each fast-forward. Never force-push it.
- DO NOT COMMIT TO `develop`, `main`, or `master`. WORK ON A NEW BRANCH.
- NEVER PUSH TO `origin/main` OR `origin/master`. STOP. ASK YOUR HUMAN FOR FURTHER INSTRUCTIONS.
- IF THE LOCAL AND REMOTE BRANCH NAMES DO NOT MATCH, STOP. ASK YOUR HUMAN FOR FURTHER INSTRUCTION. DO NOT PROCEED.

## Commit Hygiene

- Use conventional commits.
- If multiple items are needed inside the message body, bullet point them.
- Keep each item short & concise.
- Cut puffery and exposition. Say what it does, not how it feels.
- Always check which changes are staged/unstaged before committing. Are there any added by a human that might leak into your commit?
- Always aim to commit when the project can compile & run, and all tests are green. In exceptional circumstances, intermediate commits may bypass this rule.
- Commit forward unless told otherwise. Do not amend or modify history without explicit authorisation (except for fixing any commits you botched this turn).
- Despise WIP commits. Commit what's ready and keep the true WIP changes as a thin commit on top to later modify.
- Always commit topological changes separately from behavioural changes.
- Topological changes always commit before behavioural changes.
- You may squash multiple topological changes together into one commit.
- Topological changes must be atomic (e.g. if a file moves, the commit must include the file move and all reference updates including imports).
- Don't commit giant change blobs. Separate each major behavioural change into its own chronologically consistent commit.
- Treat commits as archeological artefacts for posterity: only keep the information important for future context, debugging, and/or nuance.
- When bumping dependencies: the semver should always increase; should not increase more than a sensible few increments; and should not change suffixes. Any of these could be a cause for concern and should be skipped until confirmed by your human.

### Format

```txt
<convention>(<scope>): <what-changed>
[<cause-and-effect>|<reason(s)-for-change>]
```

Except for dependency bumps, which should look like so:

```txt
<dependency>: <old> -> <new>
[Breaking: <breaking-changes>]

[Deprecated: <deprecations>]

[URL: <url>]
```

> `<dependecy>`: the `[version]` name used in the version file.

> `<url>`: link to releases/changelog page (preferably with a specific release or scoped release range); or (2) and artefact version index page.

## Branch Hygiene

- Use Trunk-based Development (small branches, targeting the trunk branch).
- Branches may be stacked.
- Automatically create a new stacked branch every turn (unless no changes were made in the previous turn), or when the new overarching goal is significantly/conceptually different.
- Always make a backup branch before risky changes, for example, when modifying commit history, and then verify the backup against the new branch. Delete the backup if full and complete, and not degraded. Note that it might not be identical, such as when rebasing. However, if the sum of all changes *can* be identical (no base delta), then it *must* be identical before accepting the change & deleting the backup.
- When rebasing, aim to preserve the *intent* behind both branches changes (unless redundant).
- `develop` marks the reviewed boundary: every commit at or below `develop` has been reviewed and approved, or only records an approved review's non-blocking findings or nits, or was rebased from such a commit (with a delta review where the rebase rule below requires one) and then passed the post-rebase build and tests or was exempt from them. Commits above `develop` are work in progress.
- Blocking findings are fixed, and the fix reviewed, before `develop` advances.
- After an approval with non-blocking findings, they get one fix round (or are written down in findings docs / `TODO.md` when something is stacked on top) and one delta review of the fixes. That review blocks only on blocking findings; its non-blocking findings go to `TODO.md` in a recording commit and `develop` advances, with no further round. A commit that only records non-blocking findings or nits needs no review of its own.
- Nits (wording, naming, comments) never start a review round. They are fixed only in a round a later review covers; nits from the last review of a branch, or from a plain approve, are only recorded in `TODO.md`.
- Review the branch delta (tip vs base), not each commit. Ignore issues a later commit or branch already fixed.
- Fixes get an adversarial review from a fresh context.
- Delete findings and `TODO.md` entries once done.
- Never rewrite a pushed branch; the permission classifier blocks force-push. To move a branch onto a newer `develop`, a 🔨 Code Implementer rebases it in the branch's checkout (the owner, or a fresh one with a handoff note); the director only fast-forwards and pushes `develop`. Create `<name>-<n>` from the branch, rebase it onto `develop`, and push it to a remote branch named `<name>-<n>`; the director deletes the old remote branch once `develop` advances. A rebase that changes the branch delta (conflict resolution) needs a delta review of that change, unless every conflict is one where both sides only add entries to `decisions-pending.md` or `docs/TODO.md`, resolved by keeping both; a conflict where either side deletes or modifies an entry gets a delta review. After any rebase, the build and tests must pass on the rebased branch, run in the checkout where the rebase was done, by the implementer that rebased it or a 🔍 Code Reviewer pointed at that checkout; this is skipped when the branch delta touches only `decisions-pending.md` or `docs/TODO.md`. A commit rebased from an approved or record-only one, cleanly or under the carve-out above, counts as approved once that run passes (or is exempt). A branch started after another has landed is named for its own purpose, not with a suffix.
- Fixes take priority over features unless the owner chooses a feature.

### Format

```txt
[<convention>/]change-purpose
```
