# Agents.md

> For repository-level consistency, these files take priority over and/or override any global instructions that may also be on the user's machine.

## Technical Writing
- Cut puffery & exposition.
- Say what it does, not how it feels.
- Never write in code comments or documentation what something _used_ to do (that belongs in git commit bodies!). If I cared how you got there, I would check the git history. What I care about is the reason _why_ it's in the current state / why it is necessary to function the way it does, without any historical reference.

## Progressive Disclosure

- Before high-level planning, read [`GOAL.md`](docs/GOAL.md), [`SPEC`](docs/SPEC.md), and [`DECISION`](docs/DECISION.md).
- For agreed terminology, read [`GLOSSARY.md`](docs/GLOSSARY.md).
- Before adding or changing a surface kind (terminal, editor, file browser, ...), read [`PLUGINS.md`](docs/PLUGINS.md).
- Before mutating source control, read [`GIT.md`](docs/GIT.md)!

## Roles & Skills

- Sessions start as 🎬 Director. Roles live in `.claude/agents/`; the roster and how to take a role are in [`.claude/agents/README.md`](.claude/agents/README.md).
- Conditional rules (building, toolchain pins, web host, sandbox, verifying the app, flagging decisions for the owner) live in micro-skills under `.claude/skills/` and load when their description matches the task.

## Editing Agents.md

> Agents.md is a record of **mistakes**.

Record each lesson in the narrowest place: a micro-skill (`.claude/skills/`) when it applies only in some situations, a role file when it applies to one role, this file only when every task needs it.

You may edit these files if:

1. You encounter a recurring tooling issue that obstructs your progress.
2. You took more than 3 turns to discover something non-obvious.
3. You repeatedly mis-interpreted or misunderstood something.
4. Your human had to correct you on a repeated mistake.
5. You have changed something that affects the accuracy of these Markdown files.
