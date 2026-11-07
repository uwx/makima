-- Shared discovery for the inherit plugin: where each other harness keeps
-- rules, skills, MCP servers, memories and conversations on disk, and how to
-- parse the formats that are not plain markdown or JSON.
--
-- Every function is read-only and returns plain tables so the plugin core can
-- stay about presentation. Paths follow each tool's own layout as implemented
-- by oh-my-pi's discovery providers, which is the closest thing to a
-- cross-harness reference.

local M = {}

-- The largest transcript we are willing to read into Lua memory (see the
-- plugin's memory cap). Listing never reads bodies, so this only gates import.
M.MAX_TRANSCRIPT_BYTES = 24 * 1024 * 1024

local function home()
  return maki.uv.os_homedir()
end

local function env(name)
  local ok, value = pcall(maki.uv.os_getenv, name)
  return ok and value or nil
end

local function exists(path)
  local ok = path and maki.fs.metadata(path)
  return ok ~= nil
end

local function is_dir(path)
  local meta = path and maki.fs.metadata(path)
  return meta ~= nil and meta.is_dir == true
end

local function is_file(path)
  local meta = path and maki.fs.metadata(path)
  return meta ~= nil and meta.is_file == true
end

local function join(...)
  return maki.fs.joinpath(...)
end

local function read(path)
  if not is_file(path) then
    return nil
  end
  return maki.fs.read(path)
end

local function trim(text)
  return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function read_json(path)
  local text = read(path)
  if not text then
    return nil
  end
  local ok, value = pcall(maki.json.decode, text)
  return ok and value or nil
end

local function sorted_entries(path, kind)
  local entries = maki.fs.dir(path)
  if not entries then
    return {}
  end
  local out = {}
  for _, entry in ipairs(entries) do
    if entry[2] == kind then
      out[#out + 1] = entry[1]
    end
  end
  table.sort(out)
  return out
end


-- Walks up from `dir` to the first ancestor that looks like a project root.
-- Each harness documents a slightly different stop rule; they all agree that
-- `.git` ends the search, and that a missing `.git` still means "this dir".
local function ancestors(dir)
  local out = {}
  local current = dir
  while current do
    out[#out + 1] = current
    if exists(join(current, ".git")) then
      break
    end
    local parent = maki.fs.dirname(current)
    if not parent or parent == current then
      break
    end
    current = parent
  end
  return out
end

-- Splits leading `---` frontmatter off a markdown file. Returns the raw
-- frontmatter text and the body. A file without frontmatter returns an empty
-- frontmatter string (never nil, so callers can tell "none" from "failed" and
-- the body stays the first return value of the tuple).
function M.split_frontmatter(text)
  local open_end = text:match("^%-%-%-\r?\n()")
  if not open_end then
    return "", text
  end
  -- The closing delimiter is `---` on its own line. `closing` lands on the
  -- newline that ends the last frontmatter line; the body starts after the
  -- closing line's own newline.
  local closing = text:find("\n%-%-%-[ \t]*\r?\n", open_end)
  if not closing then
    return "", text
  end
  local newline = text:find("\r?\n", closing + 4)
  if not newline then
    return "", text
  end
  local frontmatter = text:sub(open_end, closing - 1)
  local body = text:sub(newline + 1)
  return frontmatter, (body:gsub("^\r?\n", ""))
end

local function unquote(value)
  return (value:gsub("^[\"']", ""):gsub("[\"']$", ""))
end

-- Agent Skills frontmatter (`name:` / `description:`), used by every harness
-- that ships a `skills/<name>/SKILL.md` tree.
local function parse_skill(path)
  local text = read(path)
  if not text then
    return nil
  end
  local frontmatter, body = M.split_frontmatter(text)
  local has_frontmatter = frontmatter ~= ""
  local name = frontmatter:match("\nname:%s*([^\r\n]*)")
  local description = frontmatter:match("\ndescription:%s*([^\r\n]*)") or ""
  return {
    name = unquote(name or maki.fs.basename(maki.fs.dirname(path)) or "skill"),
    description = unquote(trim(description)),
    content = body,
    frontmatter = has_frontmatter and frontmatter or nil,
    location = path,
  }
end


-- Collects `skills/<name>/SKILL.md` under `root`, first path wins on a name
-- clash, matching how makima's own skill tool resolves duplicates.
local function scan_skills(root, into, seen)
  for _, dirname in ipairs(sorted_entries(root, "directory")) do
    local path = join(root, dirname, "SKILL.md")
    if is_file(path) then
      local skill = parse_skill(path)
      if skill and not seen[skill.name] then
        seen[skill.name] = true
        into[#into + 1] = skill
      end
    end
  end
end

-- Flattens an arbitrary on-disk MCP server entry to the shape makima writes
-- into its own `mcp.toml`: `{ command = {...} }` or `{ url, headers }`.
-- Returns nil for an entry we cannot express (bad transport, no endpoint).
local function normalize_mcp(server)
  if type(server) ~= "table" then
    return nil
  end
  if server.enabled == false or server.disabled == true then
    return nil
  end
  local command = server.command
  if type(command) == "table" then
    command = { table.unpack(command) }
  elseif type(command) == "string" then
    command = { command }
  end
  if command and #command > 0 then
    for _, arg in ipairs(server.args or {}) do
      command[#command + 1] = tostring(arg)
    end
    local environment = {}
    for key, value in pairs(server.env or server.environment or {}) do
      if type(value) == "string" then
        environment[key] = value
      end
    end
    return { command = command, environment = next(environment) and environment or nil }
  end
  local url = server.url or server.serverUrl
  if type(url) == "string" and url:match("^https?://") then
    local headers = {}
    for key, value in pairs(server.headers or {}) do
      if type(value) == "string" then
        headers[key] = value
      end
    end
    return { url = url, headers = next(headers) and headers or nil }
  end
  return nil
end

-- Reads `mcpServers` (Claude/Cursor/Windsurf/Gemini style) or `mcp.servers`
-- (VS Code style) from one JSON file and merges into `into`.
local function add_json_mcp(into, path)
  local data = read_json(path)
  if not data then
    return
  end
  local servers = data.mcpServers or (data.mcp and data.mcp.servers) or data.servers
  if type(servers) ~= "table" then
    return
  end
  for name, server in pairs(servers) do
    if into[name] == nil then
      local normalized = normalize_mcp(server)
      if normalized then
        into[name] = normalized
      end
    end
  end
end

-- Codex keeps MCP servers as `[mcp_servers.<name>]` in a TOML file. There is
-- no TOML decoder in the Lua API, so only the fields a server needs are
-- scraped: command, args, url, and inline env.
local function add_toml_mcp(into, path)
  local text = read(path)
  if not text then
    return
  end
  local current
  for line in text:gmatch("([^\n]*)") do
    local section = line:match("^%s*%[mcp_servers%.([%w%-_]+)%]")
    if section then
      current = section
      into[current] = into[current] or {}
    elseif current then
      local command = line:match('^%s*command%s*=%s*"([^"]*)"')
      local url = line:match('^%s*url%s*=%s*"([^"]*)"')
      local args = line:match("^%s*args%s*=%s*%[(.*)%]")
      if command then
        into[current].command = { command }
      elseif url then
        into[current].url = url
      elseif args then
        local list = {}
        for arg in args:gmatch('"([^"]*)"') do
          list[#list + 1] = arg
        end
        if into[current].command then
          for _, arg in ipairs(list) do
            into[current].command[#into[current].command + 1] = arg
          end
        end
      end
    end
  end
end

-- TOML basic-string quoting. `string.format("%q", s)` emits Lua escapes
-- (`\n`, `\t`, `\ddd`) that TOML rejects, so Windows paths and newlines in a
-- scraped value would produce a file makima refuses to load.
local TOML_ESCAPES = {
  ['\\'] = '\\\\',
  ['"'] = '\\"',
  ['\n'] = '\\n',
  ['\r'] = '\\r',
  ['\t'] = '\\t',
}

function M.toml_string(value)
  return '"' .. tostring(value):gsub('[\\"\n\r\t]', TOML_ESCAPES) .. '"'
end

-- Renders one server as its `[mcp.<name>]` table.
function M.mcp_block(name, server)
  local out = { "[mcp." .. name .. "]" }
  if server.command then
    local quoted = {}
    for i, value in ipairs(server.command) do
      quoted[i] = M.toml_string(value)
    end
    out[#out + 1] = "command = [" .. table.concat(quoted, ", ") .. "]"
    if server.environment then
      local keys = {}
      for key in pairs(server.environment) do
        keys[#keys + 1] = key
      end
      table.sort(keys)
      local parts = {}
      for _, key in ipairs(keys) do
        parts[#parts + 1] = key .. " = " .. M.toml_string(server.environment[key])
      end
      out[#out + 1] = "environment = { " .. table.concat(parts, ", ") .. " }"
    end
  else
    out[#out + 1] = "url = " .. M.toml_string(server.url)
    if server.headers then
      local keys = {}
      for key in pairs(server.headers) do
        keys[#keys + 1] = key
      end
      table.sort(keys)
      local parts = {}
      for _, key in ipairs(keys) do
        parts[#parts + 1] = key .. " = " .. M.toml_string(server.headers[key])
      end
      out[#out + 1] = "headers = { " .. table.concat(parts, ", ") .. " }"
    end
  end
  return table.concat(out, "\n")
end

-- Appends new servers to an existing mcp.toml. Existing tables are never
-- rewritten, so the user's edits and credentials survive a re-run.
function M.merge_mcp_toml(existing, servers)
  local names = {}
  for name in pairs(servers) do
    if not existing:find("[mcp." .. name .. "]", 1, true) then
      names[#names + 1] = name
    end
  end
  table.sort(names)
  if #names == 0 then
    return existing, {}
  end
  local trimmed = existing:gsub("%s+$", "")
  local out = { trimmed }
  for _, name in ipairs(names) do
    out[#out + 1] = ""
    out[#out + 1] = "# imported by makima /inherit"
    out[#out + 1] = M.mcp_block(name, servers[name])
  end
  return table.concat(out, "\n") .. "\n", names
end

-- Every MCP server discoverable on this machine, keyed by a sanitized name.
-- Earlier sources win, so the user's own makima config always beats a
-- foreign one (the caller adds makima's servers first).
function M.mcp_servers(cwd)
  local sides = {}
  local h = home()
  local function side(name)
    sides[name] = sides[name] or {}
    return sides[name]
  end

  if h then
    local claude = env("CLAUDE_CONFIG_DIR") or join(h, ".claude")
    add_json_mcp(side("claude"), join(claude, "mcp.json"))
    local desktop = read_json(join(h, ".claude.json"))
    if desktop and type(desktop.mcpServers) == "table" then
      local into = side("claude")
      for name, server in pairs(desktop.mcpServers) do
        if into[name] == nil then
          local normalized = normalize_mcp(server)
          if normalized then
            into[name] = normalized
          end
        end
      end
    end
    if desktop and type(desktop.projects) == "table" then
      local into = side("claude")
      for _, project in pairs(desktop.projects) do
        if type(project) == "table" and type(project.mcpServers) == "table" then
          for name, server in pairs(project.mcpServers) do
            if into[name] == nil then
              local normalized = normalize_mcp(server)
              if normalized then
                into[name] = normalized
              end
            end
          end
        end
      end
    end
    add_json_mcp(side("cursor"), join(h, ".cursor", "mcp.json"))
    add_json_mcp(side("windsurf"), join(h, ".codeium", "windsurf", "mcp_config.json"))
    add_json_mcp(side("gemini"), join(h, ".gemini", "settings.json"))
    add_toml_mcp(side("codex"), join(h, ".codex", "config.toml"))
    local copilot = env("COPILOT_HOME") or join(h, ".copilot")
    add_json_mcp(side("copilot"), join(copilot, "mcp-config.json"))
    add_json_mcp(side("opencode"), join(h, ".config", "opencode", "opencode.json"))
    add_json_mcp(side("omp"), join(h, ".omp", "agent", "mcp.json"))
  end

  -- Cline keeps its MCP settings in the VS Code global storage of whichever
  -- editor is installed.
  local appdata = env("APPDATA")
  if appdata then
    for _, editor in ipairs({ "Code", "Code - Insiders", "VSCodium", "Cursor" }) do
      add_json_mcp(
        side("cline"),
        join(appdata, editor, "User", "globalStorage", "saoudrizwan.claude-dev", "settings", "cline_mcp_settings.json")
      )
    end
  end

  local into = {}
  local order = {
    "claude",
    "cursor",
    "windsurf",
    "vscode",
    "gemini",
    "codex",
    "copilot",
    "cline",
    "opencode",
    "omp",
  }
  order[#order + 1] = "project"
  local project = side("project")
  add_json_mcp(project, join(cwd, ".mcp.json"))
  add_json_mcp(project, join(cwd, "mcp.json"))
  add_json_mcp(project, join(cwd, ".vscode", "mcp.json"))
  add_json_mcp(project, join(cwd, ".cursor", "mcp.json"))
  add_json_mcp(project, join(cwd, ".gemini", "settings.json"))
  add_json_mcp(project, join(cwd, ".claude", "mcp.json"))
  add_json_mcp(project, join(cwd, ".claude", ".mcp.json"))

  for _, name in ipairs(order) do
    for server_name, server in pairs(sides[name] or {}) do
      local sanitized = server_name:gsub("[^%w%-]", "-"):sub(1, 40)
      if sanitized == "" then
        sanitized = "server"
      end
      if into[sanitized] == nil then
        into[sanitized] = server
        into[sanitized].origin_source = name
        into[sanitized].origin_name = server_name
      end
    end
  end
  return into
end

-- MCP server entries makima itself already declares, so the planner never
-- offers to import something that is already configured.
function M.makima_mcp_names(cwd)
  local names = {}
  local function add_from(path)
    if not is_file(path) then
      return
    end
    local text = read(path)
    for name in text:gmatch("%[mcp%.([%w%-_]+)%]") do
      names[name] = true
    end
  end
  local config = maki.env.config_dir()
  if config then
    add_from(join(config, "mcp.toml"))
  end
  add_from(join(cwd, ".makima", "mcp.toml"))
  return names
end

-- Rules: markdown files that steer the model, one list of paths per source.
-- makima already loads the first of AGENTS.md / CLAUDE.md / .cursorrules /
-- .windsurfrules / .clinerules / GEMINI.md etc. from the project root itself,
-- so only the files it does not read are returned here.
function M.rules(cwd)
  local out = {}
  local seen = {}
  local h = home()
  local function add(source, path, label)
    if is_file(path) and not seen[path] then
      seen[path] = true
      out[#out + 1] = { source = source, path = path, label = label or maki.fs.basename(path) }
    end
  end

  -- Files without frontmatter are instruction-grade; makima has no directory
  -- rules mechanism, so they are offered as one aggregate.
  local roots = ancestors(cwd)
  local root_files = {
    { name = ".cursorrules", source = "cursor" },
    { name = ".windsurfrules", source = "windsurf" },
    { name = ".clinerules", source = "cline" },
    { name = "CONVENTIONS.md", source = "shared" },
    { name = "CODING_AGENT.md", source = "shared" },
  }
  for _, root in ipairs(roots) do
    for _, file in ipairs(root_files) do
      local path = join(root, file.name)
      if file.name == ".clinerules" and is_dir(path) then
        for _, md in ipairs(sorted_entries(path, "file")) do
          if md:match("%.md$") then
            add("cline", join(path, md), ".clinerules/" .. md)
          end
        end
      else
        add(file.source, path, file.name)
      end
    end
    for _, md in ipairs(sorted_entries(join(root, ".cursor", "rules"), "file")) do
      if md:match("%.mdc$") or md:match("%.md$") then
        add("cursor", join(root, ".cursor", "rules", md), md)
      end
    end
    for _, md in ipairs(sorted_entries(join(root, ".windsurf", "rules"), "file")) do
      if md:match("%.md$") then
        add("windsurf", join(root, ".windsurf", "rules", md), md)
      end
    end
    for _, md in ipairs(sorted_entries(join(root, ".github", "instructions"), "file")) do
      if md:match("%.instructions%.md$") then
        add("copilot", join(root, ".github", "instructions", md), md)
      end
    end
    if is_file(join(root, ".github", "copilot-instructions.md")) then
      add("copilot", join(root, ".github", "copilot-instructions.md"), "copilot-instructions.md")
    end
    if is_dir(join(root, ".github", "instructions")) then
      local nested = maki.fs.dir(join(root, ".github", "instructions"), { depth = 3 })
      for _, entry in ipairs(nested or {}) do
        if entry[2] == "file" and entry[1]:match("%.instructions%.md$") then
          add("copilot", join(root, ".github", "instructions", entry[1]), entry[1])
        end
      end
    end
  end

  if h then
    add("windsurf", join(h, ".codeium", "windsurf", "memories", "global_rules.md"), "global_rules.md")
    add("claude", join(h, ".claude", "CLAUDE.md"), "~/.claude/CLAUDE.md")
    local copilot = env("COPILOT_HOME") or join(h, ".copilot")
    add("copilot", join(copilot, "copilot-instructions.md"), "~/.copilot/copilot-instructions.md")
    add("codex", join(h, ".codex", "AGENTS.md"), "~/.codex/AGENTS.md")
    add("gemini", join(h, ".gemini", "GEMINI.md"), "~/.gemini/GEMINI.md")
  end
  return out
end

-- The project instruction files makima's own loader already reads: the first
-- match in each directory from the git root down to cwd. Importing those again
-- would pay for the same text twice on every request.
function M.already_loaded_rules(cwd)
  local names = {
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    "COPILOT.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
    "GEMINI.md",
    "CODING_AGENT.md",
  }
  local loaded = {}
  for _, root in ipairs(ancestors(cwd)) do
    for _, name in ipairs(names) do
      local path = join(root, name)
      if is_file(path) then
        loaded[path] = true
        break
      end
    end
    local local_file = join(root, "AGENTS.local.md")
    if is_file(local_file) then
      loaded[local_file] = true
    end
  end
  return loaded
end

-- Skills across every harness, deduplicated by name (first wins).
function M.skills(cwd)
  local out = {}
  local seen = {}
  local h = home()
  if h then
    scan_skills(join(h, ".claude", "skills"), out, seen)
    scan_skills(join(h, ".codex", "skills"), out, seen)
    scan_skills(join(h, ".config", "opencode", "skills"), out, seen)
    scan_skills(join(h, ".agents", "skills"), out, seen)
    scan_skills(join(h, ".omp", "agent", "skills"), out, seen)
    local copilot = env("COPILOT_HOME") or join(h, ".copilot")
    scan_skills(join(copilot, "skills"), out, seen)
    -- Claude Code marketplace plugins ship `skills/<name>/SKILL.md` too.
    local installed = read_json(join(h, ".claude", "plugins", "installed_plugins.json"))
    if installed and type(installed.plugins) == "table" then
      for _, entries in pairs(installed.plugins) do
        for _, entry in ipairs(entries) do
          if type(entry.installPath) == "string" then
            scan_skills(join(entry.installPath, "skills"), out, seen)
          end
        end
      end
    end
  end
  for _, root in ipairs(ancestors(cwd)) do
    scan_skills(join(root, ".claude", "skills"), out, seen)
    scan_skills(join(root, ".github", "skills"), out, seen)
    scan_skills(join(root, ".codex", "skills"), out, seen)
    scan_skills(join(root, ".cursor", "skills"), out, seen)
    scan_skills(join(root, ".opencode", "skills"), out, seen)
    scan_skills(join(root, ".agents", "skills"), out, seen)
  end
  return out
end

-- Claude Code memories: `<dir>/<name>.md` with YAML frontmatter, indexed by a
-- MEMORY.md line per file. Only memories for this project are imported.
function M.memories(cwd)
  local out = {}
  local h = home()
  if not h then
    return out
  end
  -- Claude encodes a project path by replacing every non-alphanumeric byte
  -- with `-` (`C:\Users\x` -> `C--Users-x`), so the inverse lookup is exact.
  local target = cwd:gsub("[^%w]", "-")
  local root = join(h, ".claude", "projects")
  for _, project_dir in ipairs(sorted_entries(root, "directory")) do
    local memory_dir = join(root, project_dir, "memory")
    if is_dir(memory_dir) then
      if project_dir == target then
        for _, md in ipairs(sorted_entries(memory_dir, "file")) do
          if md:match("%.md$") and md ~= "MEMORY.md" then
            local text = read(join(memory_dir, md))
            if text then
              local name = text:match("\nname:%s*(.-)\r?\n") or md:gsub("%.md$", "")
              local description = text:match("\ndescription:%s*(.-)\r?\n") or ""
              local body = text:gsub("^%-%-%-\r?\n.-?\r?\n%-%-%-\r?\n", "", 1)
              out[#out + 1] = {
                source = "claude",
                name = name,
                description = description:gsub('^"', ""):gsub('"$', ""),
                content = trim(body),
                path = join(memory_dir, md),
              }
            end
          end
        end
      end
    end
  end
  return out
end

-- Claude encodes a project path as every non-alphanumeric byte replaced with
-- `-` (`C:\Users\x` -> `C--Users-x`). The same encoding names both Claude's
-- and oh-my-pi's project session directories, so it is how a listing is tied
-- back to a project without reading transcript bodies.
function M.encode_project(path)
  return (path:gsub("[^%w]", "-"))
end

local function normalize_project(path)
  local value = path:gsub("\\", "/"):gsub("/+$", "")
  return value:lower()
end

-- VS Code and Continue store a workspace as a percent-encoded URI
-- (`file:///w%3A/projects/x`); transcripts store a plain path. A plain path is
-- returned unchanged. Remote workspaces yield nil, and a multi-root
-- `.code-workspace` file decodes to a path that matches no project directory.
function M.dir_from_uri(value)
  if type(value) ~= "string" or value == "" then
    return nil
  end
  if not value:match("^%a[%w+.-]*://") then
    return value
  end
  local decoded = value:gsub("%%(%x%x)", function(hex)
    return string.char(tonumber(hex, 16))
  end)
  return decoded:match("^file:///(.+)$")
end

-- Whether a listed conversation belongs to `cwd`. Sources that encode the
-- directory in a folder name (Claude, oh-my-pi) are compared by that encoding,
-- the rest by the directory they recorded. A source whose project cannot be
-- known without reading a large transcript body (Cline, Codex) reports none,
-- and is left out of bulk imports rather than pulling in every chat on the
-- machine; it stays available one at a time from the picker.
function M.same_project(entry, cwd)
  if entry.encoded then
    -- Windows paths are case-insensitive, and each session records the cwd as
    -- it was typed, so the encoded directory names can differ only in case.
    local got = entry.encoded:gsub("^%-+", ""):gsub("%-+$", ""):lower()
    local want = M.encode_project(cwd):gsub("^%-+", ""):gsub("%-+$", ""):lower()
    return got == want
  end
  if entry.cwd then
    return normalize_project(entry.cwd) == normalize_project(cwd)
  end
  return false
end

-- Conversation files: enough to list them without reading bodies.
M.conversations = {}

-- Claude Code: `$CLAUDE_CONFIG_DIR|~/.claude/projects/<encoded>/<id>.jsonl`.
function M.conversations.claude(cwd)
  local h = home()
  if not h then
    return {}
  end
  local root = join(env("CLAUDE_CONFIG_DIR") or join(h, ".claude"), "projects")
  local out = {}
  for _, project_dir in ipairs(sorted_entries(root, "directory")) do
    local dir = join(root, project_dir)
    for _, name in ipairs(sorted_entries(dir, "file")) do
      if name:match("%.jsonl$") then
        local path = join(dir, name)
        local meta = maki.fs.metadata(path)
        out[#out + 1] = {
          source = "claude",
          id = name:gsub("%.jsonl$", ""),
          path = path,
          cwd = project_dir:gsub("%-", "/"),
          encoded = project_dir,
          mtime = meta and meta.mtime or 0,
          size = meta and meta.size or 0,
        }
      end
    end
  end
  return out
end

-- Codex: `<root>/sessions/**/rollout-*.jsonl`, metadata in the first record.
function M.conversations.codex(cwd)
  local h = home()
  if not h then
    return {}
  end
  local root = join(h, ".codex", "sessions")
  local out = {}
  for _, year in ipairs(sorted_entries(root, "directory")) do
    for _, month in ipairs(sorted_entries(join(root, year), "directory")) do
      for _, day in ipairs(sorted_entries(join(root, year, month), "directory")) do
        for _, name in ipairs(sorted_entries(join(root, year, month, day), "file")) do
          if name:match("%.jsonl$") then
            local path = join(root, year, month, day, name)
            local meta = maki.fs.metadata(path)
            out[#out + 1] = {
              source = "codex",
              id = name:gsub("%.jsonl$", ""),
              path = path,
              mtime = meta and meta.mtime or 0,
              size = meta and meta.size or 0,
            }
          end
        end
      end
    end
  end
  return out
end

-- oh-my-pi: the rollout's first `session` record carries id, cwd and title.
function M.conversations.omp(cwd)
  local h = home()
  if not h then
    return {}
  end
  local root = join(h, ".omp", "agent", "sessions")
  local out = {}
  for _, project_dir in ipairs(sorted_entries(root, "directory")) do
    for _, name in ipairs(sorted_entries(join(root, project_dir), "file")) do
      if name:match("%.jsonl$") then
        local path = join(root, project_dir, name)
        local meta = maki.fs.metadata(path)
        out[#out + 1] = {
          source = "omp",
          id = name:gsub("%.jsonl$", ""),
          path = path,
          encoded = project_dir,
          mtime = meta and meta.mtime or 0,
          size = meta and meta.size or 0,
        }
      end
    end
  end
  return out
end

-- Gemini CLI: `~/.gemini/tmp/<hash>/chats/session-*.json`, with the project
-- root in a sibling `.project_root` file when the CLI wrote one.
function M.conversations.gemini(cwd)
  local h = home()
  if not h then
    return {}
  end
  local root = join(h, ".gemini", "tmp")
  local out = {}
  for _, hash in ipairs(sorted_entries(root, "directory")) do
    local dir = join(root, hash)
    local project = read(join(dir, ".project_root"))
    if project then
      project = project:gsub("%s+$", "")
    end
    for _, name in ipairs(sorted_entries(join(dir, "chats"), "file")) do
      if name:match("^session%-.+%.json$") then
        local path = join(dir, "chats", name)
        local meta = maki.fs.metadata(path)
        out[#out + 1] = {
          source = "gemini",
          id = name:gsub("%.json$", ""),
          path = path,
          cwd = project and project ~= "" and project or nil,
          mtime = meta and meta.mtime or 0,
          size = meta and meta.size or 0,
        }
      end
    end
  end
  return out
end

-- Cline: one task directory per conversation in VS Code global storage.
function M.conversations.cline(cwd)
  local appdata = env("APPDATA")
  if not appdata then
    return {}
  end
  local out = {}
  for _, editor in ipairs({ "Code", "Code - Insiders", "VSCodium", "Cursor" }) do
    local root = join(appdata, editor, "User", "globalStorage", "saoudrizwan.claude-dev", "tasks")
    for _, task in ipairs(sorted_entries(root, "directory")) do
      local history = join(root, task, "ui_messages.json")
      if is_file(history) then
        local meta = maki.fs.metadata(history)
        out[#out + 1] = {
          source = "cline",
          id = task,
          path = history,
          mtime = meta and meta.mtime or 0,
          size = meta and meta.size or 0,
        }
      end
    end
  end
  return out
end

-- Copilot Chat (VS Code): `workspaceStorage/<hash>/chatSessions/<id>.jsonl`.
function M.conversations.copilot(cwd)
  local appdata = env("APPDATA")
  if not appdata then
    return {}
  end
  local out = {}
  for _, editor in ipairs({ "Code", "Code - Insiders", "VSCodium" }) do
    local root = join(appdata, editor, "User", "workspaceStorage")
    for _, workspace in ipairs(sorted_entries(root, "directory")) do
      local dir = join(root, workspace, "chatSessions")
      local folder = read_json(join(root, workspace, "workspace.json"))
      for _, name in ipairs(sorted_entries(dir, "file")) do
        if name:match("%.jsonl$") or name:match("%.json$") then
          local path = join(dir, name)
          local meta = maki.fs.metadata(path)
          out[#out + 1] = {
            source = "copilot",
            id = name:gsub("%.jsonl$", ""):gsub("%.json$", ""),
            path = path,
            cwd = folder and M.dir_from_uri(folder.folder or folder.workspace),
            mtime = meta and meta.mtime or 0,
            size = meta and meta.size or 0,
          }
        end
      end
    end
  end
  return out
end

-- Continue: `~/.continue/sessions/<id>.json` with its own history array.
function M.conversations.continue(cwd)
  local h = home()
  if not h then
    return {}
  end
  local root = join(h, ".continue", "sessions")
  local out = {}
  for _, name in ipairs(sorted_entries(root, "file")) do
    if name:match("%.json$") and name ~= "sessions.json" then
      local path = join(root, name)
      local meta = maki.fs.metadata(path)
      local doc = read_json(path)
      out[#out + 1] = {
        source = "continue",
        id = name:gsub("%.json$", ""),
        path = path,
        cwd = doc and M.dir_from_uri(doc.workspaceDirectory),
        mtime = meta and meta.mtime or 0,
        size = meta and meta.size or 0,
      }
    end
  end
  return out
end

M.CONVERSATION_SOURCES = {
  { name = "claude", scan = M.conversations.claude },
  { name = "codex", scan = M.conversations.codex },
  { name = "omp", scan = M.conversations.omp },
  { name = "gemini", scan = M.conversations.gemini },
  { name = "cline", scan = M.conversations.cline },
  { name = "copilot", scan = M.conversations.copilot },
  { name = "continue", scan = M.conversations.continue },
}

-- Every listed conversation, newest first, filtered to `source`, and to the
-- conversations recorded for `cwd` when `only_project` is set. The project
-- filter is what keeps a bulk import from pulling in every chat the machine
-- ever wrote; a conversation whose project cannot be read without the body is
-- excluded, and remains available through the unfiltered picker.
function M.list_conversations(cwd, source, only_project)
  local out = {}
  for _, entry in ipairs(M.CONVERSATION_SOURCES) do
    if not source or source == entry.name then
      for _, conversation in ipairs(entry.scan(cwd)) do
        if not only_project or M.same_project(conversation, cwd) then
          out[#out + 1] = conversation
        end
      end
    end
  end
  table.sort(out, function(a, b)
    return a.mtime > b.mtime
  end)
  return out
end

return M
