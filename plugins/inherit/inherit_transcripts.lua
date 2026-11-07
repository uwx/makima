-- Converts another harness's transcript into makima's own session records.
--
-- makima stores sessions as an append-only JSONL file: a `header` record, one
-- `msg` record per message, and a trailing `meta` record. Messages are
-- `{ role = "user"|"assistant", content = [blocks] }` where a block is
-- `{type="text",text=...}`, `{type="thinking",thinking=...}`,
-- `{type="tool_use",id=...,name=...,input=...}` or
-- `{type="tool_result",tool_use_id=...,content=...,is_error=...}`.
--
-- The converters below produce exactly that shape. Anything a source cannot
-- express is dropped rather than guessed, because a malformed history makes
-- the whole session unloadable.

local M = {}

local MAX_TEXT_BYTES = 200 * 1024

local function clip(text)
  if type(text) ~= "string" then
    return nil
  end
  if #text <= MAX_TEXT_BYTES then
    return text
  end
  return text:sub(1, MAX_TEXT_BYTES) .. "\n… (truncated)"
end

-- Every harness mostly stores content as a block array, but a few records
-- (oh-my-pi's older messages, Continue's assistant turns) keep a plain string.
-- Normalizing here keeps every converter from needing its own string branch.
local function content_list(content)
  if type(content) == "string" then
    return { { type = "text", text = content } }
  end
  if type(content) == "table" then
    return content
  end
  return {}
end

local function decode_jsonl(text)
  local out = {}
  for line in text:gmatch("([^\n]*)\n?") do
    if line ~= "" then
      local ok, value = pcall(maki.json.decode, line)
      if ok and type(value) == "table" then
        out[#out + 1] = value
      end
    end
  end
  return out
end

local function text_block(text)
  local clipped = clip(text)
  return clipped and { type = "text", text = clipped } or nil
end

-- Appends one turn after dropping empty blocks. Consecutive turns of the same
-- role are left as separate messages on purpose: makima's own history has one
-- assistant message per model call, and merging would detach a tool_use from
-- the tool_result that answers it.
local function push_message(messages, role, blocks)
  local kept = {}
  for _, block in ipairs(blocks or {}) do
    if block then
      kept[#kept + 1] = block
    end
  end
  if #kept > 0 then
    messages[#messages + 1] = { role = role, content = kept }
  end
end

-- Claude Code: records carry `message.content` arrays with Anthropic block
-- shapes, so most blocks pass through unchanged.
function M.claude(text)
  local messages = {}
  local title
  local cwd
  for _, record in ipairs(decode_jsonl(text)) do
    -- The first record that names the directory wins. Messages carry it too,
    -- so this is read before the type dispatch rather than as a branch of it.
    if not cwd and type(record.cwd) == "string" then
      cwd = record.cwd
    end
    if record.type == "custom-title" then
      title = record.customTitle
    elseif record.type == "ai-title" and not title then
      title = record.aiTitle
    elseif (record.type == "user" or record.type == "assistant")
      and record.isSidechain ~= true
      and record.isMeta ~= true
      and type(record.message) == "table"
    then
      local content = record.message.content
      local blocks = {}
      if type(content) == "string" then
        blocks[1] = text_block(content)
      elseif type(content) == "table" then
        for _, block in ipairs(content) do
          if type(block) == "table" then
            if block.type == "text" then
              blocks[#blocks + 1] = text_block(block.text)
            elseif block.type == "thinking" then
              blocks[#blocks + 1] = { type = "thinking", thinking = clip(block.thinking) or "" }
            elseif block.type == "tool_use" and block.id and block.name then
              blocks[#blocks + 1] = {
                type = "tool_use",
                id = block.id,
                name = block.name,
                input = block.input or {},
              }
            elseif block.type == "tool_result" and block.tool_use_id then
              local result = block.content
              if type(result) == "table" then
                local parts = {}
                for _, part in ipairs(result) do
                  if type(part) == "table" and part.type == "text" then
                    parts[#parts + 1] = part.text
                  end
                end
                result = table.concat(parts, "\n")
              end
              blocks[#blocks + 1] = {
                type = "tool_result",
                tool_use_id = block.tool_use_id,
                content = clip(result) or "",
                is_error = block.is_error == true,
              }
            end
          end
        end
      end
      push_message(messages, record.type, blocks)
    end
  end
  return { messages = messages, title = title, cwd = cwd }
end

-- Codex rollouts interleave `response_item` and `event_msg` records. The
-- response items carry the canonical turns (user text, assistant text,
-- function calls and outputs), so event messages only fill gaps.
function M.codex(text)
  local messages = {}
  local title
  local cwd
  for _, record in ipairs(decode_jsonl(text)) do
    local payload = record.payload
    if record.type == "session_meta" and type(payload) == "table" then
      title = title or payload.title
      cwd = cwd or payload.cwd
    elseif record.type == "response_item" and type(payload) == "table" then
      if payload.type == "message" then
        local parts = {}
        for _, item in ipairs(payload.content or {}) do
          if type(item) == "table" and (item.type == "input_text" or item.type == "output_text" or item.type == "text") then
            parts[#parts + 1] = item.text
          end
        end
        push_message(messages, payload.role == "assistant" and "assistant" or "user", { text_block(table.concat(parts, "\n")) })
      elseif payload.type == "function_call" or payload.type == "custom_tool_call" then
        local id = payload.call_id or payload.id
        if id and payload.name then
          local input = payload.arguments or payload.input
          if type(input) == "string" then
            local ok, decoded = pcall(maki.json.decode, input)
            input = ok and decoded or { input = input }
          end
          push_message(messages, "assistant", {
            { type = "tool_use", id = id, name = payload.name, input = type(input) == "table" and input or {} },
          })
        end
      elseif payload.type == "function_call_output" or payload.type == "custom_tool_call_output" then
        local id = payload.call_id or payload.id
        if id then
          push_message(messages, "user", {
            { type = "tool_result", tool_use_id = id, content = clip(payload.output) or "", is_error = false },
          })
        end
      end
    elseif record.type == "event_msg" and type(payload) == "table" and payload.type == "user_message" then
      push_message(messages, "user", { text_block(payload.message) })
    end
  end
  return { messages = messages, title = title, cwd = cwd }
end

-- oh-my-pi rollouts already use the canonical block shape under `message`.
-- A tool result travels in a message whose `role` is `toolResult`, which
-- makima has no role for: it becomes a user turn holding a tool_result block,
-- the same shape Claude Code writes.
function M.omp(text)
  local messages = {}
  local title
  local cwd
  for _, record in ipairs(decode_jsonl(text)) do
    if record.type == "session" then
      title = record.title
      cwd = record.cwd
    elseif record.type == "title" and not title then
      title = record.title
    elseif record.type == "message" and type(record.message) == "table" then
      local message = record.message
      local role = message.role
      local blocks = {}
      for _, block in ipairs(content_list(message.content)) do
        if type(block) == "table" then
          if block.type == "text" then
            blocks[#blocks + 1] = text_block(block.text)
          elseif block.type == "thinking" then
            blocks[#blocks + 1] = { type = "thinking", thinking = clip(block.thinking) or "" }
          elseif block.type == "toolCall" and block.id and block.name then
            blocks[#blocks + 1] = {
              type = "tool_use",
              id = block.id,
              name = block.name,
              input = block.arguments or {},
            }
          end
        end
      end
      if role == "toolResult" then
        blocks = { { type = "tool_result", tool_use_id = message.toolCallId or "", content = clip(message.text) or "" } }
      end
      push_message(messages, role == "assistant" and "assistant" or "user", blocks)
    end
  end
  return { messages = messages, title = title, cwd = cwd }
end

-- Gemini CLI chat snapshots: a flat `messages` array, role in `type`.
function M.gemini(text)
  local ok, data = pcall(maki.json.decode, text)
  if not ok or type(data) ~= "table" or type(data.messages) ~= "table" then
    return { messages = {} }
  end
  local messages = {}
  for _, message in ipairs(data.messages) do
    if type(message) == "table" then
      local blocks = {}
      if message.type == "gemini" then
        for _, thought in ipairs(message.thoughts or {}) do
          if type(thought) == "table" then
            blocks[#blocks + 1] = { type = "thinking", thinking = clip(thought.description) or "" }
          end
        end
        blocks[#blocks + 1] = text_block(message.content)
        for _, call in ipairs(message.toolCalls or {}) do
          if type(call) == "table" and call.id and call.name then
            blocks[#blocks + 1] = { type = "tool_use", id = call.id, name = call.name, input = call.args or {} }
          end
        end
      else
        blocks[1] = text_block(message.content)
      end
      push_message(messages, message.type == "gemini" and "assistant" or "user", blocks)
    end
  end
  return { messages = messages }
end

-- Continue's session files keep a `history` array of `{ message = ... }`.
function M.continue(text)
  local ok, data = pcall(maki.json.decode, text)
  if not ok or type(data) ~= "table" or type(data.history) ~= "table" then
    return { messages = {}, title = type(data) == "table" and data.title or nil }
  end
  local messages = {}
  for _, entry in ipairs(data.history) do
    local message = type(entry) == "table" and entry.message or nil
    if type(message) == "table" then
      local blocks = {}
      if type(message.content) == "string" then
        blocks[1] = text_block(message.content)
      else
        for _, block in ipairs(message.content or {}) do
          if type(block) == "table" and block.type == "text" then
            blocks[#blocks + 1] = text_block(block.text)
          end
        end
      end
      push_message(messages, message.role == "assistant" and "assistant" or "user", blocks)
    end
  end
  return { messages = messages, title = data.title, cwd = data.workspaceDirectory }
end

-- Cline keeps a UI event log; user turns and assistant text are enough for a
-- readable transcript, tool chatter is left out on purpose.
function M.cline(text)
  local ok, data = pcall(maki.json.decode, text)
  if not ok or type(data) ~= "table" then
    return { messages = {} }
  end
  local messages = {}
  local title
  local cwd
  for _, event in ipairs(data) do
    if type(event) == "table" and type(event.text) == "string" then
      if event.type == "say" and event.say == "task" then
        title = title or event.text
        push_message(messages, "user", { text_block(event.text) })
      elseif event.type == "say" and event.say == "text" then
        push_message(messages, "assistant", { text_block(event.text) })
      elseif event.type == "say" and event.say == "user_feedback" then
        push_message(messages, "user", { text_block(event.text) })
      elseif event.type == "say" and event.say == "api_req_started" and not cwd then
        -- The environment block is embedded in the request text, not a field.
        cwd = event.text:match("Current Working Directory %(([^%)]+)%)")
      end
    end
  end
  return { messages = messages, title = title, cwd = cwd }
end

-- Copilot Chat sessions are an operation log: kind 0 is the base state, kind 1
-- sets a key at a path, kind 2 appends a value at a path. Replaying them
-- rebuilds each request's `message` and `response` array. JSON numbers arrive
-- as floats, and Lua treats `t[0]` and `t[1]` as different keys, so a path
-- segment is only used as an index when it is a whole number.
-- Copilot's path indexes are 0-based (requests[0] is the first request) and
-- Lua's are 1-based, so a whole-number segment is shifted by one.
local function array_index(key)
  local n = tonumber(key)
  if n and n >= 0 and n == math.floor(n) then
    return n + 1
  end
  return nil
end

local function key_of(table_value, key)
  local index = type(table_value) == "table" and array_index(key)
  return index or key
end

local function walk(root, path)
  local current = root
  for i = 1, #path do
    current = current[key_of(current, path[i])]
    if type(current) ~= "table" then
      return nil
    end
  end
  return current
end

local function set_at(root, path, value)
  local parent = walk(root, { table.unpack(path, 1, #path - 1) })
  if parent then
    parent[key_of(parent, path[#path])] = value
  end
end

-- A kind-2 write appends its value at the path. When the value is itself an
-- array (Copilot writes `k:["requests"], v:[...request...]`), each element is
-- appended, which is how a new request enters the log. A missing slot becomes
-- a fresh array rather than being dropped, since the first write to a path can
-- arrive before anything has created it.
local function append_at(root, path, value)
  local parent = walk(root, { table.unpack(path, 1, #path - 1) })
  if not parent then
    return
  end
  local key = key_of(parent, path[#path])
  local list = parent[key]
  if type(list) ~= "table" then
    list = {}
    parent[key] = list
  end
  if type(value) == "table" then
    for _, item in ipairs(value) do
      list[#list + 1] = item
    end
  else
    list[#list + 1] = value
  end
end

function M.copilot(text)
  local state
  for _, record in ipairs(decode_jsonl(text)) do
    local kind = record.kind
    if kind == 0 then
      state = record.v
    elseif type(record.k) == "table" and state then
      if kind == 2 then
        append_at(state, record.k, record.v)
      else
        set_at(state, record.k, record.v)
      end
    end
  end
  if type(state) ~= "table" then
    return { messages = {} }
  end
  local messages = {}
  local title = state.customTitle
  for _, request in ipairs(state.requests or {}) do
    if type(request) == "table" then
      local prompt = type(request.message) == "table" and request.message.text
      push_message(messages, "user", { text_block(prompt) })
      local blocks = {}
      for _, item in ipairs(request.response or {}) do
        if type(item) == "table" then
          if item.kind == "thinking" then
            blocks[#blocks + 1] = { type = "thinking", thinking = clip(item.value) or "" }
          elseif item.kind == "toolInvocationSerialized" and item.toolCallId then
            local label = item.invocationMessage
            if type(label) == "table" then
              label = label.value
            end
            blocks[#blocks + 1] = {
              type = "tool_use",
              id = item.toolCallId,
              name = item.toolId or "tool",
              input = { description = clip(label) or "" },
            }
          elseif item.kind == nil and type(item.value) == "string" then
            blocks[#blocks + 1] = text_block(item.value)
          end
        end
      end
      push_message(messages, "assistant", blocks)
    end
  end
  return { messages = messages, title = title }
end

M.CONVERTERS = {
  claude = M.claude,
  codex = M.codex,
  omp = M.omp,
  gemini = M.gemini,
  continue = M.continue,
  cline = M.cline,
  copilot = M.copilot,
}

-- Builds the record table for one transcript, so a caller that wants to write
-- its own JSONL (or a test) works from the same data as `session_jsonl`.
function M.session_records(converted, id, cwd, model, now)
  local messages = converted.messages or {}
  if #messages == 0 then
    return nil, "no messages"
  end
  local title = converted.title
  if type(title) ~= "string" or title == "" then
    title = nil
    for _, message in ipairs(messages) do
      if message.role == "user" then
        for _, block in ipairs(message.content) do
          if block.type == "text" then
            title = block.text
            break
          end
        end
      end
      if title then
        break
      end
    end
  end
  title = (title or "Imported session"):gsub("%s+", " "):sub(1, 60)

  local records = {
    { t = "header", v = 2, id = id, model = model, cwd = cwd, created_at = now },
  }
  for _, message in ipairs(messages) do
    records[#records + 1] = { t = "msg", d = message }
  end
  records[#records + 1] = {
    t = "meta",
    title = title,
    token_usage = { input_tokens = 0, output_tokens = 0, cache_creation_input_tokens = 0, cache_read_input_tokens = 0 },
    updated_at = now,
  }
  return records, title
end

-- makima's own writer starts every record with `{"t":"header"`, `{"t":"msg"`
-- or `{"t":"meta"` and its session scanner matches those bytes as a prefix to
-- count messages and find the title. maki's JSON encoder orders object keys,
-- so `t` would not come first; the prefix is therefore written by hand and
-- only the payload goes through the encoder.
local function string_field(value)
  return maki.json.encode(value)
end

function M.session_jsonl(converted, id, cwd, model, now)
  local records, title = M.session_records(converted, id, cwd, model, now)
  if not records then
    return nil, title
  end
  local lines = {}
  for i, record in ipairs(records) do
    local encoded, err
    if record.t == "header" then
      encoded = string.format(
        '{"t":"header","v":%d,"id":%s,"model":%s,"cwd":%s,"created_at":%d}',
        record.v,
        string_field(record.id),
        string_field(record.model),
        string_field(record.cwd),
        record.created_at
      )
    elseif record.t == "msg" then
      encoded, err = maki.json.encode(record.d)
      if encoded then
        encoded = '{"t":"msg","d":' .. encoded .. "}"
      end
    elseif record.t == "meta" then
      -- Drop the encoded `"t":"meta"` member wherever the encoder placed it,
      -- then put it back at the front. The loader parses the whole object, but
      -- the session scanner needs the literal prefix to find the title.
      local body = { title = record.title, token_usage = record.token_usage, updated_at = record.updated_at }
      encoded, err = maki.json.encode(body)
      if encoded then
        encoded = '{"t":"meta",' .. encoded:sub(2)
      end
    end
    if not encoded then
      return nil, tostring(err)
    end
    lines[i] = encoded
  end
  return table.concat(lines, "\n") .. "\n", title
end

return M
