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

Type `/inherit` to browse and import one item at a time from a picker. `/inherit import-mcp` appends every discovered MCP server to `mcp.toml` in one step; restart Makima afterwards so the servers connect.

The agent can use the `inherit` tool the same way. Ask it to scan, then to import a named item. Conversations import one at a time, since each one becomes a whole session.

```text
inherits: rules 1, skills 10, mcp 4, memories 3, conversations 593
```

## Limits

- A conversation is converted to Makima's own message shape. The text, thinking, tool calls and tool results come across; tool output that a source stored separately is dropped.
- Very large transcripts (over 24 MB) are skipped rather than read into memory.
- Imported sessions record the model that was current when you imported them, not the model the other agent used.
- Imports never overwrite: a rule block is replaced in place, a skill or memory is skipped when the name exists, and an MCP server already in `mcp.toml` is left alone.
