-- Inherit what another coding harness already left on disk: rules, skills, MCP
-- servers, memories and conversations from .claude, .cursor, .windsurf,
-- .gemini, .codex, .cline, .github (Copilot), VS Code workspace storage and
-- oh-my-pi.
--
-- Nothing is read from a foreign file on every request. At load time one cheap
-- scan finds which sources exist. The data comes in through `/inherit`
-- (interactive) or the `inherit` tool (model-driven), and whatever it imports
-- lands in makima's own config, skills dir, memory store or session store,
-- where the rest of makima already knows how to read it.

local H = require("inherit_helpers")
local T = require("inherit_transcripts")

local RULE_FILE = "AGENTS.md"
local IMPORT_MARKER = "<!-- imported by makima /inherit from "
local PROJECT_SKILL_DIR = ".claude/skills"
local MAX_RULE_BYTES = 512 * 1024
-- Enough of a transcript to hold its title or first message without loading
-- a long chat whole.
local PREVIEW_BYTES = 128 * 1024
local UNTITLED = "(untitled)"
local NO_PROJECT = "project not recorded"

local function flash(msg)
  maki.ui.flash(msg)
  maki.log.info(msg)
end

-- A command or keymap invocation may have no agent target, so fall back to the
-- process cwd exactly like the bundled skill plugin does for its pickers.
local function cwd_of(ctx)
  if ctx then
    local ok, dir = pcall(ctx.resolve_path, ctx, ".")
    if ok and dir then
      return dir
    end
  end
  return maki.uv.cwd()
end

-- The model an imported session records. A command may run with no agent, and
-- the session's model is a display detail there, so a missing one is fine.
local function selection_spec(ctx)
  if not ctx then
    return "unknown"
  end
  local ok, selection = pcall(ctx.model, ctx)
  return ok and type(selection) == "table" and selection.spec or "unknown"
end

-- -- session ids -------------------------------------------------------------
--
-- makima reads a session file name as the base58 encoding of exactly 16 bytes,
-- so an imported session needs an id in that shape. Sixteen random bytes are
-- generated first and converted with a rolling base58 accumulator, which keeps
-- every value small: `acc * 256 + byte < 2^22` and each digit step divides, so
-- this never needs integer division past 32 bits.

local BASE58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

local function base58_encode(bytes)
  local acc = 0
  local digits = {}
  for _, byte in ipairs(bytes) do
    acc = acc * 256 + byte
    for i = #digits, 1, -1 do
      acc = acc + digits[i] * 256
      digits[i] = acc % 58
      acc = math.floor(acc / 58)
    end
    while acc > 0 do
      table.insert(digits, 1, acc % 58)
      acc = math.floor(acc / 58)
    end
  end
  if #digits == 0 then
    return "1"
  end
  local out = {}
  for i, digit in ipairs(digits) do
    out[i] = BASE58:sub(digit + 1, digit + 1)
  end
  return table.concat(out)
end

-- The id is a hash of the source transcript path, so importing the same
-- conversation twice maps to the same file and the second pass skips it
-- instead of filling the session list with duplicates.
local function avalanche(state)
  state = bit32.bxor(state, bit32.lshift(state, 13))
  state = bit32.bxor(state, bit32.rshift(state, 17))
  state = bit32.bxor(state, bit32.lshift(state, 5))
  return state
end

local function session_id_for(key)
  -- Two 32-bit lanes over the path. The multiplier keeps `state * 1000003`
  -- below 2^53 so doubles stay exact; the lanes together give 64 bits, which
  -- makes a collision between two different transcripts implausible.
  local bytes = {}
  local n = 0
  for _, seed in ipairs({ 2166136261, 374761393 }) do
    local state = seed
    for i = 1, string.len(key) do
      state = (state * 1000003 + key:byte(i)) % 4294967296
    end
    if state == 0 then
      state = 0x9e3779b9
    end
    for _ = 1, 8 do
      state = avalanche(state)
      n = n + 1
      bytes[n] = state % 256
    end
  end
  -- Byte 0 is forced to 1..255 so the encoding always decodes to 16 bytes with
  -- no leading-zero ambiguity (base58 pads a leading zero with a literal "1").
  bytes[1] = bytes[1] % 255 + 1
  return base58_encode(bytes)
end

-- -- import writers ----------------------------------------------------------

-- Appends a foreign rule to the global makima AGENTS.md inside a marked block.

-- A block already there is replaced, so re-running is idempotent.
local function import_rule(rule)
  local dir = maki.env.config_dir()
  if not dir then
    return nil, "cannot resolve makima config dir"
  end
  local text, err = maki.fs.read(rule.path)
  if not text then
    return nil, "read error: " .. tostring(err)
  end
  if #text > MAX_RULE_BYTES then
    return nil, "larger than " .. MAX_RULE_BYTES .. " bytes"
  end
  local path = maki.fs.joinpath(dir, RULE_FILE)
  local existing = maki.fs.read(path) or ""
  local marker = IMPORT_MARKER .. rule.path .. " -->"
  local block = marker .. "\n" .. text:gsub("%s+$", "") .. "\n<!-- /imported -->\n"
  local start_at = existing:find(marker, 1, true)
  if start_at then
    local close_at = existing:find("<!-- /imported -->", start_at, true)
    local resume = close_at and existing:find("\n", close_at, true)
    if resume then
      existing = existing:sub(1, start_at - 1) .. block .. existing:sub(resume + 1)
    else
      existing = existing:sub(1, start_at - 1) .. block
    end
  else
    existing = existing .. (existing == "" and "" or "\n") .. block
  end
  maki.fs.mkdir(dir, { parents = true })
  local ok, werr = maki.fs.atomic_write(path, existing)
  if not ok then
    return nil, "write error: " .. tostring(werr)
  end
  return true
end

-- Skills go to `<project>/.claude/skills/<name>/SKILL.md`, a tree makima's own
-- skill tool already scans. Source frontmatter is kept as it was.
local function import_skill(skill, cwd)
  local target = maki.fs.joinpath(cwd, PROJECT_SKILL_DIR, skill.name, "SKILL.md")
  if maki.fs.metadata(target) then
    return nil, "already exists"
  end
  local body = skill.content
  if not skill.frontmatter then
    body = "---\nname: "
      .. skill.name
      .. "\ndescription: "
      .. (skill.description or "")
      .. "\n---\n"
      .. body
  elseif not skill.frontmatter:match("\ndescription:") and skill.description ~= "" then
    body = "---\n" .. skill.frontmatter .. "\ndescription: " .. skill.description .. "\n---\n" .. body
  end
  local ok, err = maki.fs.mkdir(maki.fs.dirname(target), { parents = true })
  if ok then
    ok, err = maki.fs.atomic_write(target, body)
  end
  if not ok then
    return nil, "write error: " .. tostring(err)
  end
  return true
end

-- Memories reuse makima's own on-disk format, so `/memory` and the memory tool
-- pick them up with no extra glue.
local function import_memory(memory)
  local ok, helpers = pcall(require, "memory_helpers")
  if not ok then
    return nil, "memory plugin is disabled"
  end
  local state = maki.env.state_dir()
  if not state then
    return nil, "cannot resolve makima state dir"
  end
  local root = maki.fs.root(maki.uv.cwd(), ".git") or maki.uv.cwd()
  local memories = maki.fs.joinpath(state, "projects", helpers.project_id(root), "memories")
  local name = memory.name:gsub("[^%w%-_]", "-"):sub(1, 60) .. ".md"
  local path = maki.fs.joinpath(memories, name)
  if maki.fs.metadata(path) then
    return nil, "already exists"
  end
  maki.fs.mkdir(memories, { parents = true })
  local content = helpers.encode_frontmatter({ "imported" }) .. memory.content
  local wok, err = maki.fs.atomic_write(path, content)
  if not wok then
    return nil, "write error: " .. tostring(err)
  end
  return true
end

local function sessions_dir()
  local state = maki.env.state_dir()
  return state and maki.fs.joinpath(state, "sessions")
end

-- Writes one converted transcript as a fresh session and returns its id. The
-- transcript's own project wins when it records one, so a chat resumes in the
-- directory it belongs to; otherwise it falls back to the project the command
-- ran in, unless `require_project` is set and it is skipped instead.
local function import_conversation(conversation, cwd, model, require_project)
  local convert = T.CONVERTERS[conversation.source]
  if not convert then
    return nil, "no converter for " .. conversation.source
  end
  if conversation.size > H.MAX_TRANSCRIPT_BYTES then
    return nil, "transcript is larger than " .. (H.MAX_TRANSCRIPT_BYTES / 1048576) .. " MB"
  end
  local dir = sessions_dir()
  if not dir then
    return nil, "cannot resolve makima state dir"
  end
  local id = session_id_for(conversation.path)
  local target = maki.fs.joinpath(dir, id .. ".jsonl")
  if maki.fs.metadata(target) then
    return nil, "already imported"
  end
  local text, err = maki.fs.read(conversation.path)
  if not text then
    return nil, "read error: " .. tostring(err)
  end
  maki.fs.mkdir(dir, { parents = true })
  local converted = convert(text) or {}
  -- Gemini keeps the project beside the transcript rather than inside it, so a
  -- scan that found one hands it over.
  local project = H.dir_from_uri(converted.cwd or conversation.cwd)
  if not project and require_project then
    return nil, "transcript records no project directory"
  end
  local jsonl, title = T.session_jsonl(converted, id, project or cwd, model, os.time())
  if not jsonl then
    return nil, title
  end
  local ok, werr = maki.fs.atomic_write(target, jsonl)
  if not ok then
    return nil, "write error: " .. tostring(werr)
  end
  return id, title
end

-- -- shared plan -------------------------------------------------------------

local function build_plan(cwd)
  local loaded = H.already_loaded_rules(cwd)
  local rules = {}
  for _, rule in ipairs(H.rules(cwd)) do
    if not loaded[rule.path] then
      rules[#rules + 1] = rule
    end
  end
  local configured = H.makima_mcp_names(cwd)
  local mcp = {}
  for name, server in pairs(H.mcp_servers(cwd)) do
    server.name = name
    if not configured[name] then
      mcp[#mcp + 1] = server
    end
  end
  table.sort(mcp, function(a, b)
    return a.name < b.name
  end)
  return {
    rules = rules,
    skills = H.skills(cwd),
    mcp = mcp,
    memories = H.memories(cwd),
    conversations = H.list_conversations(cwd),
  }
end

-- -- import actions ----------------------------------------------------------

local function import_mcp(servers)
  if #servers == 0 then
    return nil, "no new MCP servers found"
  end
  local dir = maki.env.config_dir()
  if not dir then
    return nil, "cannot resolve makima config dir"
  end
  local path = maki.fs.joinpath(dir, "mcp.toml")
  local by_name = {}
  for _, server in ipairs(servers) do
    by_name[server.name] = server
  end
  local existing = maki.fs.read(path) or ""
  local merged, added = H.merge_mcp_toml(existing, by_name)
  if #added == 0 then
    return nil, "already configured"
  end
  maki.fs.mkdir(dir, { parents = true })
  local ok, err = maki.fs.atomic_write(path, merged)
  if not ok then
    return nil, "write error: " .. tostring(err)
  end
  return added
end

local function tally_line(kind, items, apply)
  local done, skipped = 0, 0
  for _, item in ipairs(items) do
    local ok = apply(item)
    if ok then
      done = done + 1
    else
      skipped = skipped + 1
    end
  end
  return string.format("%s: %d imported, %d skipped", kind, done, skipped)
end

-- Imports this project's conversations. Each session records the directory its
-- transcript names, so a chat resumes where it belongs.
local function import_project_conversations(cwd, model)
  local conversations = H.list_conversations(cwd, nil, true)
  return tally_line("conversations", conversations, function(conversation)
    return import_conversation(conversation, cwd, model) ~= nil
  end)
end

-- Imports every conversation on the machine, each into the project it recorded.
-- A transcript that names no project is skipped: there is nowhere to resume it,
-- and guessing this project would mix unrelated chats into it. Rows that name a
-- directory that no longer exists are skipped for the same reason.
local function import_every_conversation(model)
  local conversations = H.list_conversations(nil, nil, false)
  local done, skipped_dup, skipped = 0, 0, 0
  local reasons = {}
  for _, conversation in ipairs(conversations) do
    -- The second return is the title on success and an error on failure, so
    -- only the id (and a successful call) says which happened.
    local id, reason = import_conversation(conversation, nil, model, true)
    if id then
      done = done + 1
    elseif reason == "already imported" then
      skipped_dup = skipped_dup + 1
    else
      -- A chat with no project, an unreadable one and an oversized one are
      -- different outcomes, so each is reported rather than summed.
      skipped = skipped + 1
      reasons[tostring(reason)] = (reasons[tostring(reason)] or 0) + 1
    end
  end
  local summary = string.format("conversations: %d imported, %d already imported, %d skipped", done, skipped_dup, skipped)
  local details = {}
  for reason, count in pairs(reasons) do
    details[#details + 1] = string.format("%s (%d)", reason, count)
  end
  if #details > 0 then
    table.sort(details)
    summary = summary .. ": " .. table.concat(details, ", ")
  end
  return summary
end

-- Runs every importer once. Rules, skills and memories skip anything already
-- present, and MCP only appends servers the config does not have, so a second
-- run is a no-op rather than a duplicate. Conversations are limited to this
-- project; every chat on the machine is its own subcommand. Returns a per-kind
-- tally so the caller can report what happened.
local function inherit_all(cwd, model)
  local plan = build_plan(cwd)
  local tally = {
    tally_line("rules", plan.rules, import_rule),
    tally_line("skills", plan.skills, function(skill)
      return import_skill(skill, cwd)
    end),
    tally_line("memories", plan.memories, import_memory),
  }

  if #plan.mcp > 0 then
    local added, err = import_mcp(plan.mcp)
    tally[#tally + 1] = added and ("mcp: " .. #added .. " added") or ("mcp: " .. tostring(err))
  else
    tally[#tally + 1] = "mcp: nothing new"
  end

  tally[#tally + 1] = import_project_conversations(cwd, model)
  return tally
end

-- The project a conversation records, tidied for display: sources that encode
-- it in a folder name lose the drive colon and gain repeated slashes.
local function project_line(cwd)
  if not cwd then
    return NO_PROJECT
  end
  return (cwd:gsub("/+", "/"):gsub("^(%a)/", "%1:/"))
end

local function pick_and_apply(cwd, items, apply, describe)
  if #items == 0 then
    maki.ui.flash("Nothing to inherit here")
    return
  end
  local rows = {}
  for _, item in ipairs(items) do
    rows[#rows + 1] = {
      label = item.name or item.label or item.id,
      detail = describe(item),
      section = item.source,
      age = item.age,
    }
  end
  local event = maki.ui.open_list_picker(rows, {
    title = " Import ",
    footer = { { "Enter", "import" }, { "Esc", "close" } },
  })
  if event.type ~= "choice" then
    return
  end
  local ok, err = apply(items[event.index])
  if ok then
    local imported = items[event.index]
    flash("Imported " .. (imported.name or imported.label or imported.id))
  else
    flash("Import failed: " .. tostring(err))
  end
end

-- One table of subcommands, so an unknown argument is answered rather than
-- silently opening the picker: a typo used to look like `/inherit` ignoring it.
local SUBCOMMANDS = {
  ["import-mcp"] = function(cwd)
    local added, err = import_mcp(build_plan(cwd).mcp)
    if added then
      flash("Added " .. table.concat(added, ", ") .. " to mcp.toml; restart makima")
    else
      flash("MCP import: " .. tostring(err))
    end
  end,
  all = function(cwd, model)
    local summary = table.concat(inherit_all(cwd, model), "; ")
    if summary:find("mcp: %d+ added") then
      summary = summary .. ". Restart makima to connect the new MCP servers"
    end
    maki.log.info("inherit all: " .. summary)
    flash(summary)
  end,
  conversations = function(_, model)
    local summary = import_every_conversation(model)
    maki.log.info("inherit conversations: " .. summary)
    flash(summary)
  end,
}

local function run_picker(cwd, model)
  local plan = build_plan(cwd)
  local items = {
    { label = "Rules (" .. #plan.rules .. ")", detail = "foreign rule files makima does not load itself" },
    { label = "Skills (" .. #plan.skills .. ")", detail = "SKILL.md trees from every harness" },
    { label = "MCP servers (" .. #plan.mcp .. ")", detail = "append to ~/.config/makima/mcp.toml" },
    { label = "Memories (" .. #plan.memories .. ")", detail = "Claude Code project memories" },
    { label = "Conversations (" .. #plan.conversations .. ")", detail = "import as resumable sessions" },
  }
  local event = maki.ui.open_list_picker(items, {
    title = " Inherit from other harnesses ",
    footer = { { "Enter", "choose" }, { "Esc", "close" } },
  })
  if event.type ~= "choice" then
    return
  end
  if event.index == 1 then
    pick_and_apply(cwd, plan.rules, import_rule, function(rule)
      return rule.source .. " · " .. rule.path
    end)
  elseif event.index == 2 then
    pick_and_apply(cwd, plan.skills, function(skill)
      return import_skill(skill, cwd)
    end, function(skill)
      return skill.description ~= "" and skill.description or skill.location
    end)
  elseif event.index == 3 then
    local added, err = import_mcp(plan.mcp)
    if added then
      flash("Added " .. table.concat(added, ", ") .. " to mcp.toml. Restart makima to connect.")
    else
      flash("MCP import: " .. tostring(err))
    end
  elseif event.index == 4 then
    pick_and_apply(cwd, plan.memories, import_memory, function(memory)
      return memory.description ~= "" and memory.description or memory.path
    end)
  elseif event.index == 5 then
    -- The label is the title the imported session will carry, read from the
    -- transcript's head; the id is a UUID and says nothing about the chat.
    for _, conversation in ipairs(plan.conversations) do
      local head = maki.fs.read_head(conversation.path, PREVIEW_BYTES)
      conversation.label = (head and T.preview_title(conversation.source, head)) or UNTITLED
      conversation.age = conversation.mtime > 0 and maki.time.at(math.floor(conversation.mtime)) or nil
    end
    pick_and_apply(cwd, plan.conversations, function(conversation)
      return import_conversation(conversation, cwd, model)
    end, function(conversation)
      return project_line(conversation.cwd)
    end)
  end
end

maki.api.register_command({
  name = "/inherit",
  description = "Import rules, skills, MCP servers, memories and conversations left by other coding harnesses",
  tui_only = true,
  arguments = {
    {
      name = "subcommand",
      type = "string",
      optional = true,
      completion = {
        items = {
          { label = "all", insertion = "all", description = "Import every kind at once" },
          { label = "conversations", insertion = "conversations", description = "Import every chat on the machine" },
          { label = "import-mcp", insertion = "import-mcp", description = "Append MCP servers to mcp.toml" },
        },
      },
    },
  },
  handler = function(opts, ctx)
    local cwd = cwd_of(ctx)
    local model = selection_spec(ctx)
    local sub = opts.values.subcommand
    if sub then
      local run = SUBCOMMANDS[sub]
      if not run then
        flash("Unknown /inherit argument " .. sub .. "; try all, conversations or import-mcp")
        return
      end
      run(cwd, model)
      return
    end
    run_picker(cwd, model)
  end,
})

-- -- model-facing tool -------------------------------------------------------

maki.api.register_tool({
  name = "inherit",
  kind = "read",
  description = "Discover and import configuration and history left on this machine by other coding agents (Claude Code, Cursor, Windsurf, Gemini CLI, Codex, Cline, Copilot Chat, oh-my-pi, Continue).\n\n"
    .. "- `scan` lists what is available per source.\n"
    .. "- `import` copies one kind into makima: rules into ~/.config/makima/AGENTS.md, skills into .claude/skills/<name>/SKILL.md, MCP servers into ~/.config/makima/mcp.toml, memories into the project's memory store, conversations into new resumable sessions.\n"
    .. "- `import` with `kind = \"all\"` does every kind at once; conversations are then limited to this project's own.\n"
    .. "- `import` with `kind = \"conversations-everywhere\"` imports every conversation on the machine, each session in the project its transcript recorded.\n\n"
    .. "Ask before importing, then report what changed. MCP changes need a restart.",
  schema = {
    type = "object",
    properties = {
      action = {
        type = "string",
        enum = { "scan", "import" },
        description = "What to do. Defaults to scan.",
      },
      kind = {
        type = "string",
        enum = { "all", "conversations-everywhere", "rules", "skills", "mcp", "memories", "conversations" },
        description = "What to import. Required for import; optional for scan. `all` imports every kind at once (conversations are limited to this project's); `conversations-everywhere` imports every chat on the machine into the project its transcript recorded.",
      },
      source = {
        type = "string",
        description = "Only consider this harness (claude, codex, omp, gemini, cline, copilot, continue).",
      },
      name = {
        type = "string",
        description = "Import only the entry with this name (rule label, skill name, memory name or conversation id).",
      },
    },
  },

  handler = function(input, ctx)
    local cwd, cwd_err = ctx:resolve_path(".")
    if not cwd then
      return { llm_output = "error: " .. tostring(cwd_err), is_error = true }
    end
    local plan = build_plan(cwd)
    local kind = input.kind
    local source = input.source

    if input.action ~= "import" then
      local parts = {
        "rules: " .. #plan.rules .. ", skills: " .. #plan.skills .. ", mcp: " .. #plan.mcp
          .. ", memories: " .. #plan.memories .. ", conversations: " .. #plan.conversations,
      }
      local function section(list, name)
        local lines = {}
        for _, entry in ipairs(list) do
          local key = entry.name or entry.label or entry.id
          if not source or entry.source == source then
            local extra = entry.description or entry.path or ""
            lines[#lines + 1] = "  " .. key .. " [" .. tostring(entry.source) .. "] " .. tostring(extra):sub(1, 100)
          end
        end
        parts[#parts + 1] = name .. " (" .. #lines .. "):\n" .. table.concat(lines, "\n")
      end
      if not kind or kind == "rules" then
        section(plan.rules, "rules")
      end
      if not kind or kind == "skills" then
        section(plan.skills, "skills")
      end
      if not kind or kind == "mcp" then
        section(plan.mcp, "mcp")
      end
      if not kind or kind == "memories" then
        section(plan.memories, "memories")
      end
      if not kind or kind == "conversations" then
        section(plan.conversations, "conversations")
      end
      return { llm_output = table.concat(parts, "\n\n") }
    end

    if kind == "all" then
      return { llm_output = table.concat(inherit_all(cwd, selection_spec(ctx)), "\n") }
    end

    if kind == "conversations-everywhere" then
      return { llm_output = import_every_conversation(selection_spec(ctx)) }
    end

    if kind == "mcp" then
      local added, err = import_mcp(plan.mcp)
      if not added then
        return { llm_output = "error: " .. tostring(err), is_error = true }
      end
      return { llm_output = "added " .. table.concat(added, ", ") .. " to mcp.toml; restart makima to connect them" }
    end

    local list, apply
    if kind == "rules" then
      list, apply = plan.rules, import_rule
    elseif kind == "skills" then
      list, apply = plan.skills, function(skill)
        return import_skill(skill, cwd)
      end
    elseif kind == "memories" then
      list, apply = plan.memories, import_memory
    elseif kind == "conversations" then
      -- Each conversation becomes a whole session, so importing all of them by
      -- accident would flood the session list. The caller must name one.
      if not input.name then
        return {
          llm_output = "error: import conversations one at a time; pass `name` (run a scan to list ids)",
          is_error = true,
        }
      end
      list, apply = plan.conversations, function(conversation)
        return import_conversation(conversation, cwd, selection_spec(ctx))
      end
    else
      return {
        llm_output = "error: kind must be one of all, conversations-everywhere, rules, skills, mcp, memories, conversations",
        is_error = true,
      }
    end

    local done, failed = {}, {}
    for _, entry in ipairs(list) do
      local key = entry.name or entry.label or entry.id
      if (not input.name or key == input.name) and (not source or entry.source == source) then
        local ok, err = apply(entry)
        if ok then
          done[#done + 1] = key
        else
          failed[#failed + 1] = key .. " (" .. tostring(err) .. ")"
        end
      end
    end
    if #done == 0 then
      return {
        llm_output = "nothing imported" .. (#failed > 0 and ("; failed: " .. table.concat(failed, ", ")) or ""),
        is_error = true,
      }
    end
    local msg = "imported: " .. table.concat(done, ", ")
    if #failed > 0 then
      msg = msg .. "\nfailed: " .. table.concat(failed, ", ")
    end
    return { llm_output = msg }
  end,
})
