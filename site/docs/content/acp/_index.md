+++
title = "ACP"
weight = 22
[extra]
group = "Guides"
+++

# ACP (Agent Client Protocol)

Run Makima inside your editor. `makima acp` starts an [ACP](https://agentclientprotocol.com/) server over stdio, so any ACP-capable editor (like [Zed](https://zed.dev/)) can drive Makima as its coding agent.

```bash
makima acp
```

## Zed setup

Add Makima as a custom agent in Zed's `settings.json`:

```json
"agent_servers": {
  "Makima": {
    "default_config_options": {
      "model": "deepseek/deepseek-v4-flash"
    },
    "type": "custom",
    "command": "makima",
    "args": ["acp"],
    "env": {}
  }
}
```

The `model` value is a `provider/model-id` spec, same format as `makima --model`.

## Remote agents over TCP

ACP only standardizes stdio, but the protocol is transport agnostic, so `makima acp --listen` serves the same newline delimited JSON-RPC over TCP:

```bash
makima acp --listen 0.0.0.0:7331
makima acp --listen 127.0.0.1:7331 --token my-token
```

Every connection opens with a handshake line, so a client that cannot name the token never reaches the agent:

```
client -> {"maki":"lan","version":1,"token":"..."}
server -> {"maki":"lan","version":1,"ok":true}
```

After that, ACP messages flow one JSON-RPC message per line. A refused handshake answers `{"ok":false,"error":"unauthorized"}` and closes the connection.

The token comes from `--token`, then `MAKIMA_LAN_TOKEN`, then `lan-token` in the Makima state directory, which is created on first use with owner only permissions. The command prints the token when it starts.

### Sharing the stdio session

`makima acp --share <addr>` keeps serving the client that started it over stdio and also accepts TCP clients for that same session:

```bash
makima acp --share 0.0.0.0:7331
```

The stdio connection owns the session, and watchers can attach and take control as below. Closing stdio ends the session and the process, and watchers see the connection end. `--share` and `--listen` are mutually exclusive: `--listen` serves TCP only, `--share` adds TCP beside stdio.

A process serves one session at a time. Closing the connection that owns it ends it, like closing an agent's stdin; until then another connection that asks for a new session is refused with error code `-32011`, and can watch instead.

The listener speaks plain TCP, and the token is its only gate. Use it on a network you trust or over a private network such as Tailscale, and prefer `127.0.0.1` unless another machine needs it.

### Sharing a running session

A connection that owns a session can open a listener for it later, without a restart:

```json
{"jsonrpc":"2.0","id":4,"method":"maki/share","params":{}}
```

```json
{"bind":"0.0.0.0:60667","address":"192.168.2.4:60667","link":"lan://192.168.2.4:60667","token":"..."}
```

`bind` is what the agent bound, `address` what another machine reaches, `link` the agent address a client sets, and `token` what it presents in the handshake. The token is `MAKIMA_LAN_TOKEN` when set, then the stored `lan-token`. Asking twice answers the same listener. `maki/unshare` closes it, and so does ending the connection that opened it.

Only the connection that owns the session can share it: a watching connection gets `-32014`, even while it holds control. A share that cannot bind answers `-32013`.

The desktop window has the same thing as a status bar toggle. `Share` opens the listener and shows the address with buttons for the link and the token, `Stop sharing` closes it. Another window reaches it with `MAKI_DESKTOP_REMOTE=lan://192.168.2.4:60667` and `MAKI_DESKTOP_LAN_TOKEN=...`. The link names no directory: that window watches the session the other machine serves instead of starting one.

### Watching a session

While a connection owns a session, other connections can watch it read only. `maki/live` lists what this process serves:

```json
{"sessions":[{"sessionId":"...","watchers":0}]}
```

Then send `maki/attach` with the session id:

```json
{"jsonrpc":"2.0","id":3,"method":"maki/attach","params":{"sessionId":"..."}}
```

Makima first replays what the session already emitted, as the same `session/update` notifications a client already renders, then streams new ones. The reply follows the replay:

```json
{"sessionId":"...","readOnly":true,"replayed":12,"truncated":false,"configOptions":[...]}
```

`replayed` counts the updates sent before the reply, `truncated` says the oldest ones were dropped because the session ran past the recording limit, and `configOptions` is the session's current option list, so a client can paint its status bar without asking. Several connections can watch one session.

A watching connection that does not hold control cannot write: `session/prompt`, `session/new`, `session/load`, `session/set_mode`, `session/set_config_option` and `maki/rewind` answer with error code `-32010`. It can still initialize, list sessions, switch themes and complete arguments.

### Taking control

The connection that created or loaded the session controls it at first. To move control, any connection sends `maki/take_control`, and the reply says whether it now holds it:

```json
{"controller":true}
```

Only an idle session changes hands. While a turn runs or a permission question is open, the request answers `-32012`, so no request is left without the connection that asked for it. The connection that loses control receives a notification:

```json
{"method":"maki/ui/control","params":{"controller":false}}
```

The controller prompts, cancels, answers permission questions and rewinds. The owning connection can take control back the same way.

The recording follows the session, not the connection: when the owner rewinds, watchers receive `maki/ui/rewind` and the recording starts over from the rewound transcript. When the owning connection ends, the session ends and watchers lose it.

## What works

- Sessions persist. Loading a session replays the full conversation in the editor. A session that is open in another terminal cannot be loaded.
- Session options include Model, YOLO, Fast, Workflow, and available plugin options such as Bash auto mode. New and loaded sessions return the complete ordered option list. Option updates also contain the complete list.
- Model switching uses the editor's model selector. All configured providers appear. Providers such as OpenRouter add discovered models in the background. A model change and any required Fast disablement appear as one update.
- Modes switch between build mode and plan mode. Build mode has full tool access. Plan mode limits writes to the plan file.
- Tool permission prompts appear in the editor. The user can allow or reject the operation once or always.
- The `question` tool becomes an ACP elicitation form. If the client does not support elicitation, Makima removes the tool and the model asks in plain text.
- Tool progress streams during execution, including sub-agents and batched calls.
- Prompts can include images and editor-attached files.
- The editor receives the target-scoped command list. The list updates when plugin or MCP registrations change. An available leading slash command runs before model input. Unknown and unavailable slash-prefixed text is rejected with an error. Prefix a literal message that starts with `/` with another slash to send it as text: `//lmao` sends `/lmao`. Agent-turn commands preserve images and text resources. Local commands reject non-text content.
- A `session/prompt` sent while a turn runs joins that turn, as typing does in the TUI (see [messages and turns](/docs/agent-work/#messages-and-turns)). Both prompts resolve when the turn ends, with the same stop reason. A prompt that cannot join, for example after a model switch, runs as the next turn and resolves with it. While `/compact` or `/btw` runs, a new prompt is rejected.
- `/compact` reports a `Compact context` tool call and saves the compacted model history before it reports success.
- `/btw` runs one isolated model request with copied history and no tools. Its question and response do not change the primary model history.
- `/cd` reports the canonical working directory as an agent message. Later relative tool locations use that directory.

ACP maps the target-scoped central registry to ACP command metadata. Commands that require TUI capabilities are omitted from ACP's advertised command list. Invoking an unavailable command returns an error; to send it as a literal prompt, escape the leading slash (`//help` sends `/help`). ACP hides `/new` and `/clear` because the ACP client owns session creation through `session/new`. A typed `/new` or `/clear` resolves locally and returns guidance to use `session/new`; it does not invoke model inference, reset model history, or mutate the active session. Custom Markdown commands, MCP prompts, and portable Lua commands use the same registry and collision rules as the TUI. See [Commands](/docs/commands/).

ACP cannot remove messages from the editor's visible transcript or change the editor's displayed session directory. `/compact` changes only the model context. `/cd` changes Makima's canonical working directory and reports it in the transcript, but the editor can continue to display its original directory field.

Authentication, providers, and permissions come from your normal Makima config. Set up [providers](/docs/providers/) first and ACP sessions just work.

```bash
makima acp
makima acp -m anthropic/claude-sonnet-4-6
makima acp --yolo
makima --no-jit acp
```

`makima acp` takes `-m` / `--model`, `--yolo`, `--listen`, `--share` and `--token` as subcommand flags. Global flags like `--no-jit` must come before the subcommand (`makima --no-jit acp`, not `makima acp --no-jit`).

When the client closes stdin, the prompt still running finishes and gets its response, and subagents still working in the background finish too, along with the turns their results start, then the server exits. Permission requests and questions that come after that are refused, since the client can no longer answer them. Send `session/cancel` first to stop the prompt instead.

A session open in another terminal is locked by it: loading fails until that instance exits, which releases the lock automatically (or a few seconds after it crashes).
