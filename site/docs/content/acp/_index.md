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

`makima --share 0.0.0.0:7331 -c <ID>` opens an existing session on the agent instead of a new one, and `-l` picks the latest session for the directory. The window names that session in its resume hint, and `makima -c <ID>` resumes it later without the listener.

The stdio connection owns the session, and other connections attach to it as below. Closing stdio ends the session and the process, and watchers see the connection end. `--share` and `--listen` are mutually exclusive: `--listen` serves TCP only, `--share` adds TCP beside stdio.

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

The terminal window starts a listener with `/share` and stops it with `/unshare`. In a window that runs the session itself, `/share` restarts the interface as a client of an agent that serves the same session, on every interface and a port the system picks, and shows the link other machines use. From there the window talks ACP like `--share` does, and `/unshare` stops the listener without ending the session; the session keeps running until the window closes.

The desktop window has the same thing as a status bar toggle. `Share` opens the listener and shows the address with buttons for the link and the token, `Stop sharing` closes it. Another window reaches it with `MAKI_DESKTOP_REMOTE=lan://192.168.2.4:60667` and `MAKI_DESKTOP_LAN_TOKEN=...`, or by choosing the session under `Remote` in its sidebar, which asks for the token. The same link works in the composer as `/connect lan://192.168.2.4:60667 <token>`. The link names no directory: that window watches the session the other machine serves instead of starting one.

### Finding a session on the local network

While a listener is open, the agent announces it over mDNS as `_maki._tcp`, so a client on the same network finds it without being told an address:

```
makima on LITTLESCREAMER._maki._tcp.local.
  project=makima
  version=1
  192.168.2.4:47445
```

The name is the project directory and the machine, and `project` repeats the directory for clients that show a list. The token is never announced: reaching the session still needs it, so discovery only replaces typing the address.

The announcement follows the listener. It appears when `--listen`, `--share` or `maki/share` opens one and is withdrawn when it closes. A listener bound to loopback is not announced, because no other machine can reach it.

### Watching a session

While a connection owns a session, other connections can attach to it. `maki/live` lists what this process serves:

```json
{"sessions":[{"sessionId":"...","watchers":0}]}
```

Then send `maki/attach` with the session id:

```json
{"jsonrpc":"2.0","id":3,"method":"maki/attach","params":{"sessionId":"..."}}
```

Makima first replays what the session already emitted, as the same `session/update` notifications a client already renders, then sends the history the session has committed (see [`maki/ui/committed`](#history)), then streams new updates live. The reply follows the replay:

```json
{"sessionId":"...","readOnly":false,"replayed":12,"truncated":false,"configOptions":[...]}
```

`replayed` counts the updates sent before the reply, `truncated` says the oldest ones were dropped because the session ran past the recording limit, and `configOptions` is the session's current option list, so a client can paint its status bar without asking. Several connections can attach to one session, and any of them may act on it.

`readOnly` is always false, and stays in the reply for clients that still read it. Any attached connection can prompt, cancel, answer a question, change options or rewind; the loop that owns the session takes those requests one at a time, so two windows never run two turns at once.

### Watching from a terminal

`makima attach <link>` opens a shared session in the full interface:

```bash
makima attach                           # pick from what the network announces
makima attach 192.168.2.4:60667         # token from $MAKIMA_LAN_TOKEN or the state directory
makima attach lan://192.168.2.4:60667 --token my-token
makima attach 192.168.2.4:60667 --plain # line rendering instead
```

With no link it waits a few seconds and lists the announced sessions to choose from. The chat then shows the turn you start: streamed text, thinking, tool calls, and the usual permission prompt. Each turn is a `session/prompt` sent to the machine that runs the agent, so the model, the tools and the files all stay there, and typing is all it takes to act on a session another window is showing. A turn another window starts while you watch streams into the same transcript, under a line saying it runs in another window. Those rows are a view: they belong to the session on the machine that ran the turn. That machine also says what each turn committed, so a window that attaches later starts with the transcript it missed, and every window keeps the same history to resume, rewind and compact from. `--plain` prints the replay in line mode.

`--plain` keeps the line renderer for a terminal that cannot take the full screen, which is also what a pipe gets. There `/cancel` stops a turn, `/quit` leaves, tool calls print as `→ title` and `✓ title`, and permissions and pickers take a number.

### Many windows, one session

There is no driver. Every connection attached to a session may prompt, cancel, answer a permission question, change options or rewind, and the loop that owns the session takes those requests one at a time, so two windows never run two turns at once. A prompt that arrives while a turn runs joins or queues behind it like a prompt typed locally.

A permission question goes to every window, so the person nearest any of them can answer it. Every window receives the same `session/request_permission` request, and each answers with the ACP response it already knows; the connection that owns the session takes the first answer and tells the others the question is settled:

```json
{"method":"maki/ui/permission_done","params":{"requestId":41}}
```

A window that receives it closes the question without answering. Pickers and completions still go to the window that asked for the work in flight; if that window is gone, they go to the connection that owns the session. `maki/take_control` still answers `{"controller":true}` for older clients; nothing has to be taken.

Every connection that shows the session, the owner's included, hears when a turn starts and ends, so it can label a turn another window runs and know whether Stop would do anything:

```json
{"method":"maki/ui/turn","params":{"running":true}}
```

A prompt another window sends arrives as a `user_message_chunk` update, so every window shows the question and not just the answer.

### History

When a turn settles, the session tells every window what it committed:

```json
{"method":"maki/ui/committed","params":{"sessionId":"...","messages":[...],"replace":false,"compacted":false}}
```

Append `messages` to the transcript you keep, or replace it whole when `replace` is true: the session rewound or compacted, and the list is now the complete history. `compacted` says a compaction summary is part of it. A client that just attached receives the whole history the same way, with `replace: true`, so its own copy starts where the session is instead of at the first turn it watches.

The messages are the stored form of Makima's messages, the same shape `session/load` writes. Tool results are included, so a window can draw the whole turn and not only its text. A window that watched the turn already drew it from the updates; this list is what keeps its history, and its resume, rewind and token count, identical to every other window's.

The recording follows the session, not the connection: when any window rewinds, the others receive `maki/ui/rewind` and the recording starts over from the rewound transcript. The session lives as long as the process that runs it; when the connection that owns it ends, the session ends and the others lose it.

### Changing what future turns use

A window that keeps its own copy of the session's config, like the terminal interface, sends what it changed back to the agent:

```json
{"jsonrpc":"2.0","id":7,"method":"session/set_config_option","params":{"sessionId":"...","configId":"model","value":"openai/gpt-5"}}
```

The window compares the option list after each change of its own and sends only the built-ins that moved: model, thinking, fast, workflow, yolo. Plugin options stay on the side that owns them. If the agent refuses the value, the window says so and keeps running on the config the agent still has.

### Compacting a shared session

A window that only watches a session asks the agent to compact it, because the transcript to summarize lives there:

```json
{"jsonrpc":"2.0","id":8,"method":"maki/compact","params":{"sessionId":"...","instructions":"keep the failing test names"}}
```

`instructions` is optional. The call returns when the compaction ends, with the same `stopReason` a prompt gets. Every window sees the `Compact context` tool call while it runs, and the compacted transcript arrives afterwards as a `maki/ui/committed` with `replace: true` and `compacted: true`.

A `!shell` command's output, and anything else the window showed but the user did not type, travels with the prompt instead of being typed into it:

```json
{"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{"sessionId":"...","prompt":[{"type":"text","text":"run the tests again"}],"_meta":{"maki":{"preamble":[{"role":"user","content":[{"type":"text","text":"shell preamble output: ..."}],"kind":"observation"}]}}}}
```

The messages are the stored form, in the order the agent should read them, before the prompt.

A rewind works the same way, for the same reason:

```json
{"jsonrpc":"2.0","id":9,"method":"maki/rewind","params":{"offset":4}}
```

The reply carries the `draft` the rewound turn started from, which the window puts back in its input box. The others hear `maki/ui/rewind` and rebuild from the transcript that follows.

### Moving the session's directory

`/cd` changes where the session's tools run, so it runs where the session lives, and every window is told:

```json
{"jsonrpc":"2.0","id":11,"method":"maki/chdir","params":{"sessionId":"...","path":"../other-project"}}
```

The reply carries the `cwd` the agent resolved, which is canonical and may not be the path that was typed. The session then sends what every window shows:

```json
{"method":"maki/ui/cwd","params":{"sessionId":"...","cwd":"/home/you/other-project"}}
```

A window follows it: its status bar, its file completions and its own shells use that directory. The session records the last one, so a window that attaches later gets it in the `maki/attach` reply as `cwd` and starts in the right place. A `/cd` typed as a prompt reaches every window the same way, from the loop that ran the command.

A window that follows another process's session cannot start a new conversation there, so `/new` is not in its command list and answers with guidance when typed. `/tasks`, `/mcp` and `/login` behave the same way: tasks, MCP servers and credentials belong to the process that runs the session.

### Switching the mode

Build and plan mode decide what the session may write, so a switch runs where the session lives. A window asks for it with ACP's own method:

```json
{"jsonrpc":"2.0","id":12,"method":"session/set_mode","params":{"sessionId":"...","modeId":"plan"}}
```

The session announces the new mode as `current_mode_update`, and every window follows that announcement instead of assuming its request worked:

```json
{"method":"session/update","params":{"sessionId":"...","update":{"sessionUpdate":"current_mode_update","currentModeId":"plan","_meta":{"maki":{"plan":"/home/you/project/.makima/plans/fix-the-parser.md"}}}}}
```

Plan mode carries the file the session drafts into, under the `maki` metadata key, because that file belongs to the session and not to a window. A window that sees the path uses the session's plan in its own plan view; Build mode announces no plan. Because the announcement is the session's, it can also arrive between turns, so a window listens for it on the connection rather than inside a turn. Modes saved with a resumed session arrive the same way.

### When the link drops

A client attached over TCP reconnects on its own. It retries with backoff (from 250 ms, capped at 5 s), runs `initialize` again, asks `maki/live` for the sessions the agent serves, and attaches the one it was watching. The reattach replays the session's updates and its committed history, so the transcript the client missed comes back in order, and a turn that ran while it was away shows up too.

The client clears what it draws before the replay, so the transcript is the session as it is now rather than a mix. If `maki/live` no longer lists the session, the window is told the shared session ended while it was disconnected and stops retrying. An agent started by the client itself (stdio or SSH) is not reconnected: that process is the session.

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

ACP itself cannot remove messages from the editor's visible transcript or change the editor's displayed session directory. `/compact` changes only the model context. `/cd` changes Makima's canonical working directory, reports it in the transcript, and announces it as `maki/ui/cwd`; Makima's own windows follow that, but an editor that only speaks ACP can keep displaying its original directory field.

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
