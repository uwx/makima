-- Unit coverage for the inherit plugin's two pure layers: the per-harness
-- transcript converters (they must produce message shapes makima's own session
-- loader accepts) and the small format helpers. Path discovery and the import
-- writers are exercised through the host, which the spec harness cannot set up
-- from a plugin source.

local H = require("inherit_helpers")
local T = require("inherit_transcripts")
local th = require("maki.test_helpers")

local case, eq = th.case, th.eq

local function roles(messages)
  local out = {}
  for _, message in ipairs(messages) do
    out[#out + 1] = message.role
  end
  return table.concat(out, ",")
end

local function kinds(message)
  local out = {}
  for _, block in ipairs(message.content) do
    out[#out + 1] = block.type
  end
  return table.concat(out, ",")
end

case("claude_keeps_anthropic_blocks", function()
  local jsonl = table.concat({
    '{"type":"user","message":{"role":"user","content":"hello"},"isSidechain":false}',
    '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"read","input":{"path":"a"}}]}}',
    '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"data"}]}}',
    '{"type":"custom-title","customTitle":"My chat"}',
  }, "\n")
  local result = T.claude(jsonl)
  eq(result.title, "My chat")
  eq(#result.messages, 3)
  eq(roles(result.messages), "user,assistant,user")
  eq(kinds(result.messages[2]), "thinking,text,tool_use")
  local block = result.messages[3].content[1]
  eq(block.type, "tool_result")
  eq(block.tool_use_id, "t1")
  eq(block.content, "data")
end)

case("claude_skips_sidechains_and_meta", function()
  local jsonl = table.concat({
    '{"type":"user","isSidechain":true,"message":{"content":"subagent"}}',
    '{"type":"user","isMeta":true,"message":{"content":"meta"}}',
    '{"type":"user","message":{"content":"real"}}',
  }, "\n")
  local result = T.claude(jsonl)
  eq(#result.messages, 1)
  eq(result.messages[1].content[1].text, "real")
end)

case("codex_pairs_calls_with_outputs", function()
  local jsonl = table.concat({
    '{"type":"session_meta","payload":{"id":"x","cwd":"/tmp"}}',
    '{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}',
    '{"type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"bash","arguments":"{\\"command\\":\\"ls\\"}"}}',
    '{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}',
  }, "\n")
  local result = T.codex(jsonl)
  eq(#result.messages, 3)
  eq(roles(result.messages), "user,assistant,user")
  local call = result.messages[2].content[1]
  eq(call.type, "tool_use")
  eq(call.id, "c1")
  eq(call.input.command, "ls")
  eq(result.messages[3].content[1].tool_use_id, "c1")
end)

case("omp_maps_tool_result_role", function()
  local jsonl = table.concat({
    '{"type":"session","id":"s","cwd":"/tmp","title":"omp chat"}',
    '{"type":"message","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}',
    '{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"a","name":"bash","arguments":{"command":"ls"}}]}}',
    '{"type":"message","message":{"role":"toolResult","toolCallId":"a","text":"out"}}',
  }, "\n")
  local result = T.omp(jsonl)
  eq(result.title, "omp chat")
  eq(roles(result.messages), "user,assistant,user")
  eq(result.messages[3].content[1].type, "tool_result")
  eq(result.messages[3].content[1].tool_use_id, "a")
end)

case("gemini_wraps_thoughts", function()
  local doc = '{"messages":[{"type":"user","content":"q"},{"type":"gemini","content":"a","thoughts":[{"description":"thinking"}],"toolCalls":[{"id":"g1","name":"x","args":{}}]}]}'
  local result = T.gemini(doc)
  eq(roles(result.messages), "user,assistant")
  eq(kinds(result.messages[2]), "thinking,text,tool_use")
end)

case("continue_reads_history", function()
  local doc = '{"title":"t","history":[{"message":{"role":"user","content":[{"type":"text","text":"q"}]}},{"message":{"role":"assistant","content":"a"}}]}'
  local result = T.continue(doc)
  eq(result.title, "t")
  eq(roles(result.messages), "user,assistant")
  eq(result.messages[2].content[1].text, "a")
end)

case("cline_keeps_turns_not_tool_chatter", function()
  local doc = '[{"type":"say","say":"task","text":"do it"},{"type":"say","say":"api_req_started","text":"{}"},{"type":"say","say":"text","text":"done"}]'
  local result = T.cline(doc)
  eq(result.title, "do it")
  eq(roles(result.messages), "user,assistant")
end)

case("copilot_replays_patch_log", function()
  -- Copilot appends requests as a one-element array (`v` is a list), then
  -- writes fields at the 0-based index it just added.
  local jsonl = table.concat({
    '{"kind":0,"v":{"requests":[],"sessionId":"s"}}',
    '{"kind":2,"k":["requests"],"v":[{"message":{"text":"q"},"response":[]}]}',
    '{"kind":2,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"why"}]}',
    '{"kind":2,"k":["requests",0,"response"],"v":[{"value":"answer","supportHtml":false}]}',
    '{"kind":1,"k":["customTitle"],"v":"copilot chat"}',
  }, "\n")
  local result = T.copilot(jsonl)
  eq(result.title, "copilot chat")
  eq(roles(result.messages), "user,assistant")
  eq(kinds(result.messages[2]), "thinking,text")
  eq(result.messages[2].content[2].text, "answer")
end)

case("session_jsonl_has_header_messages_meta", function()
  local converted = { messages = { { role = "user", content = { { type = "text", text = "first question" } } } } }
  local jsonl = T.session_jsonl(converted, "8AQGAut7N92awznwCnjuR", "C:/tmp", "p/m", 1234)
  local raw = {}
  for line in jsonl:gmatch("[^\n]+") do
    raw[#raw + 1] = line
  end
  eq(#raw, 3)
  -- The session scanner matches these bytes literally, so key order is part of
  -- the format, not a detail the encoder gets to pick.
  eq(raw[1]:sub(1, #'{"t":"header","v":2'), '{"t":"header","v":2')
  eq(raw[2]:sub(1, #'{"t":"msg","d":'), '{"t":"msg","d":')
  eq(raw[3]:sub(1, #'{"t":"meta",'), '{"t":"meta",')
  local lines = {}
  for i, line in ipairs(raw) do
    lines[i] = maki.json.decode(line)
  end
  eq(lines[1].cwd, "C:/tmp")
  eq(lines[1].id, "8AQGAut7N92awznwCnjuR")
  eq(lines[3].title, "first question")
  eq(lines[3].token_usage.input_tokens, 0)
end)

case("session_jsonl_rejects_empty", function()
  local jsonl, reason = T.session_jsonl({ messages = {} }, "id", "C:/tmp", "m", 1)
  eq(jsonl, nil)
  eq(reason, "no messages")
end)

case("toml_string_escapes_backslashes", function()
  eq(H.toml_string("C:\\Program Files\\x"), '"C:\\\\Program Files\\\\x"')
  eq(H.toml_string('a"b'), '"a\\"b"')
  eq(H.toml_string("line\nbreak"), '"line\\nbreak"')
end)

case("merge_mcp_toml_appends_only_new", function()
  local existing = "[mcp.old]\ncommand = [\"a\"]\n"
  local servers = {
    old = { command = { "a" } },
    fresh = { command = { "b", "--flag" } },
  }
  local merged, added = H.merge_mcp_toml(existing, servers)
  eq(#added, 1)
  eq(added[1], "fresh")
  th.has(merged, "[mcp.old]")
  th.has(merged, "[mcp.fresh]")
  th.has(merged, 'command = ["b", "--flag"]')
end)

case("split_frontmatter_separates_body", function()
  local frontmatter, body = H.split_frontmatter("---\nname: x\ndescription: y\n---\n\nbody here\n")
  if not frontmatter then
    error("frontmatter should have been found")
  end
  th.has(frontmatter, "name: x")
  eq(body, "body here\n")
end)

case("split_frontmatter_without_header", function()
  local frontmatter, body = H.split_frontmatter("plain text")
  eq(frontmatter, "")
  eq(body, "plain text")
end)

th.report()
