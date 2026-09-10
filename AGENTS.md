This is a pure fork of the original/upstream [bug-ops/mcpls](https://github.com/bug-ops/mcpls).

No changes made here will be upstreamed.
Don't read `CONTRIBUTING.md`, it bears no import unto how you should work.

This is a devkit workspace. Use devkit to create worktrees and run project tasks, including builds, tests, and cleanup. Follow the devkit harness guidance for commands and file claims.

A `commit-msg` hook enforces Conventional Commits and the subject and body limits; `devrun task hooks` installs it in a fresh checkout. An agent's commit ends with a trailer naming the model that wrote it, `Co-Authored-By: AGENT MODEL <ADDRESS>`:

- Claude Code, at `noreply@anthropic.com`: `Claude Opus 5`, `Claude Sonnet 5`, `Claude Fable 5.1`, `Claude Haiku 4.5`.
- Codex, at `codex@openai.com`: `Codex GPT-5.6-Sol`, `Codex GPT-5.5`, `Codex GPT-6-Astra`.
