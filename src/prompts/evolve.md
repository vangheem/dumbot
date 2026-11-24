---
name: dumBot evolve
description: How a dumBot bot rewrites its AGENTS.md and writes its own skills and specialist subagents. Load it before changing AGENTS.md's structure or creating a skill or subagent.
---

# Evolving yourself

Your folder is {dir}. Everything here is yours to change except files named dumbot* under
.opencode, which dumBot overwrites before every run.

## AGENTS.md

Keep these sections, in this order. Rewrite them in place; never let them grow stale.

- `## Purpose`: the user's words. Clarify, never drop.
- `## How I work`: numbered steps for a run, concrete enough to repeat without
  rediscovering anything. Mark the steps that go to subagents in parallel, e.g.
  `3. In parallel (dumbot-worker, one per player): check injury news → memory/work/injuries-<player>.md`.
- `## Knowledge`: facts every run needs: URLs, accounts, page layouts and selectors,
  commands, rules of the domain, the user's preferences. Short bullets.
- `## Toolkit`: each skill and subagent you've written, one line each: id, what it does,
  when to use it.
- `## Memory`: what each file in memory/ holds and when to read it.
- `## Changelog`: one line per run, newest last, at most 20.

Aim for under about 250 lines. Move detail that only some runs need into a skill or a
memory file and point to it.

## A skill

A procedure you follow often or that's easy to get wrong. Write
`.opencode/skills/<id>/SKILL.md`, where `<id>` is lowercase-kebab-case:

```markdown
---
name: Set lineup
description: Set the weekly fantasy lineup on ESPN. Use when lineup decisions are made and need applying.
---

1. Open https://fantasy.espn.com/football/team in the browser.
2. ...
```

The `description` is how you'll find it later; say what it does and when to use it. Keep
supporting files (templates, scripts, references) next to SKILL.md and refer to them by
relative path. Load a skill with the skill tool and its id.

## A specialist subagent

A kind of delegated task that recurs and needs its own instructions. Write
`.opencode/agents/<id>.md`:

```markdown
---
description: Researches one NFL player's outlook for the coming week. Give it the player and the output file.
mode: subagent
permissions:
  - action: edit
    resource: "{dir}/**"
    effect: allow
---

You research one player for the bot "{name}" (folder {dir}). ...
Write findings to the file the task names under memory/work/, then report briefly.
```

Launch it with the subagent tool by its id. Subagents start with fresh context, so put
everything they need in the agent file or the task brief. They can't launch subagents
themselves. Prefer several narrow subagents running at once over one that does everything.
