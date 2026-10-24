# Roles

Each `.md` here with a `name` in its frontmatter is a role. This README has none, so Claude Code treats it as documentation.

## Taking a role

- A new session in this repo starts as 🎬 Director (`agent` in `.claude/settings.json`).
- To start as another role, including for headless `claude -p` runs: `claude --agent <role>`, or set `agent` in your `.claude/settings.local.json`. The role's frontmatter (model, effort, tool limits) then applies to the session.
- Told "you are the <role>" mid-session: read `.claude/agents/<role>.md` and follow its body. Its frontmatter doesn't apply; the session keeps its own model, effort and tools.
- Dispatched with `subagent_type: "<role>"`: the body is already your system prompt.

## Roster

| Role | Does | Edits files | Default model / effort |
| :- | :- | :- | :- |
| 🎬 `director` | Talks with the owner (or its parent director), decides what a goal needs, dispatches roles, reports back | Only its `decisions-pending.md` entries | opus / high |
| 🔨 `code-implementer` | Plans and implements one unit of work in one context | Yes | opus / high |
| 🔍 `code-reviewer` | Adversarial review of a plan or a branch delta; reports, never fixes | No | opus / high |
| 📐 `role-architect` | Tunes the role system: creates, splits, merges and deletes roles and micro-skills | Roles, skills, `AGENTS.md` | opus / high |
| 🩺 `role-reviewer` | Reads transcripts for where roles struggled or erred; suggests fixes | No | opus / medium |

Defaults apply only when the dispatcher passes no `model` or `effort`. The role architect keeps this table current.

## Conventions

- Whoever writes something commits and pushes it, since it is already in their context (`docs/GIT.md`).
- Conditional rules live in micro-skills (`.claude/skills/<name>/SKILL.md`). Each skill's `description` names the situation it covers, so its body loads only then. `AGENTS.md` holds only what every task needs.

## Shared frontmatter

- `autoCompactWindow: 300000`: backstop for the context budget. Compaction drops detail, so the director hands off to a fresh agent before it triggers. Haiku's window is smaller than this, so its 100,000 budget rests on the director alone.
- `experimental.cacheTtl: 1h`: matches the director's one-hour reuse window, so a follow-up within the hour reads the agent's context from cache.
