# Git.md

## Cardinal Rules That Must Never Be Broken:

- COMMITTING AND PUSHING ARE PRE-AUTHORISED: commit and push freely, without asking. This
  project runs autonomously; no per-commit or per-push approval is needed.
- NEVER SIGN COMMITS OR TAGS: always pass `--no-gpg-sign` (`git commit --no-gpg-sign`, likewise for `merge`, `rebase`, `cherry-pick` and `revert`; `git tag --no-sign`). Signing waits for a human to unlock the key, and agents work while no human is present.
- `develop` MOVES ONLY BY FAST-FORWARD, AND ONLY ONTO COMMITS 🔍 Code Reviewer HAS APPROVED. Push it after each fast-forward. Never force-push it.
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
- `develop` marks the reviewed boundary: every commit at or below `develop` has been reviewed and approved. Commits above `develop` are work in progress.
- Blocking findings are fixed, and the fix reviewed, before `develop` advances. Non-blocking findings are fixed the same way when nothing is stacked on top; otherwise they are written down (findings docs / `TODO.md`) before `develop` advances. A commit that only records a review's findings needs no further review.
- Review the branch delta (tip vs base), not each commit. Ignore issues a later commit or branch already fixed.
- Fixes get an adversarial review from a fresh context.
- Delete findings and `TODO.md` entries once done.
- Fixes take priority over features unless the owner chooses a feature.

### Format

```txt
[<convention>/]change-purpose
```
