+++
title = "Inheriting other agents"
weight = 24
[extra]
group = "Guides"
+++

# Inheriting other agents

Switching to Makima should not mean starting from an empty config. The `inherit` plugin finds what other coding agents already left on this machine and copies it into Makima's own files, where the rest of Makima reads it like anything you wrote yourself.

Nothing foreign is read on every request. A cheap scan at startup only finds which sources exist. The data comes in when you ask for it.

## What it reads

| Kind | From | Lands in |
|------|------|----------|
| Rules | `.cursorrules`, `.windsurfrules`, `.clinerules`, `.cursor/rules/*.mdc`, `.windsurf/rules/`, `.github/instructions/*.instructions.md`, and the global copies | `~/.config/makima/AGENTS.md`, inside a marked block |
| Skills | `.claude/skills/`, `.github/skills/`, `.codex/skills/`, `.cursor/skills/`, `.agents/skills/`, and installed Claude plugin caches | `<project>/.claude/skills/<name>/SKILL.md` |
| MCP servers | `mcp.json`, `.claude.json`, `.codex/config.toml`, `.gemini/settings.json`, `.vscode/mcp.json`, Cline settings and more | appended to `~/.config/makima/mcp.toml` |
| Memories | Claude Code `<project>/memory/*.md` | the project's Makima memory store, so `/memory` sees them |
| Conversations | Claude Code, OpenAI Codex, oh-my-pi, Gemini CLI, Continue, Cline and Copilot Chat | new resumable Makima sessions |

Makima already loads the first of `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, `.windsurfrules`, `.clinerules`, `GEMINI.md` and the rest from each project directory, so those are skipped: importing them again would pay for the same text on every request.

## Using it

Type `/inherit` to browse and import one item at a time from a picker.

`/inherit all` does everything in one pass: every rule, skill, memory and MCP server, plus this project's conversations. Conversations are scoped to the project you run it from, so a machine with hundreds of unrelated chats does not turn into hundreds of sessions. If new MCP servers were added, restart Makima afterwards so they connect.

`/inherit conversations` imports every conversation on the machine instead. Each session records the directory its transcript names, so a chat resumes in the project it belongs to rather than the one you imported from. A transcript that records no project is skipped, because there is nowhere to resume it, and one whose directory no longer exists opens in that missing directory — the same refusal Makima already gives a session from another folder.

Every import records the transcript's own project when the format has one: Claude Code, oh-my-pi, Copilot, Continue and Cline all do, Gemini uses its `.project_root` file, and Cline keeps it inside the request text. Codex rollouts name it in their session metadata.

`/inherit import-mcp` is the narrower version for MCP only.

The agent can use the `inherit` tool the same way: `scan` lists what is available, `import` copies one kind, `import` with `kind = "all"` is the bulk path for this project, and `kind = "conversations-everywhere"` is the bulk path for every project.

Every import is a no-op when there is nothing new. A rule block is replaced in place, an existing skill or memory is skipped, an MCP server already in `mcp.toml` is left alone, and an already-imported conversation maps to the same session file and is skipped, so re-running `/inherit all` is safe.

```text
inherits: rules 1, skills 10, mcp 4, memories 3, conversations 117
```

## Limits

- A conversation is converted to Makima's own message shape. The text, thinking, tool calls and tool results come across; tool output that a source stored separately is dropped.
- Very large transcripts (over 24 MB) are skipped rather than read into memory.
- Imported sessions record the model that was current when you imported them, not the model the other agent used. A session whose stored model no longer resolves, or whose provider has no credentials on this machine, still opens, on the current model.
