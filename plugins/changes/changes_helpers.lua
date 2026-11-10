-- Parsing and layout for the changes panel: what `git status` reported, and
-- how a diff becomes the buffer lines below the file list.

local M = {}

local SEPARATOR = "\0"
local HUNK_PREFIX = "@@"
local ADDED_PREFIX = "+"
local REMOVED_PREFIX = "-"
local UNTRACKED = "??"
local ELLIPSIS = "…"
local MAX_LINE_CHARS = 400

local KINDS = {
  M = { "M", "warning" },
  A = { "A", "success" },
  D = { "D", "error" },
  R = { "R", "warning" },
  C = { "C", "warning" },
  U = { "U", "error" },
  ["?"] = { "?", "dim" },
  ["!"] = { "!", "dim" },
}

local FILE_HEADER_PREFIXES = {
  "diff ",
  "index ",
  "--- ",
  "+++ ",
  "new file",
  "deleted file",
  "old mode",
  "new mode",
  "similarity",
  "rename ",
  "Binary files",
}

local function starts_with(text, prefix)
  return text:sub(1, #prefix) == prefix
end

local function marker_for(code)
  local worktree = code:sub(2, 2)
  local index = code:sub(1, 1)
  local letter = worktree ~= " " and worktree or index
  return KINDS[letter] or { letter, "dim" }
end

function M.kind(code)
  local kind = marker_for(code)
  return kind[1], kind[2]
end

function M.truncate(text, width)
  if width and width > 0 and #text > width then
    if width <= #ELLIPSIS then
      return text:sub(1, width)
    end
    return text:sub(1, width - #ELLIPSIS) .. ELLIPSIS
  end
  if #text > MAX_LINE_CHARS then
    return text:sub(1, MAX_LINE_CHARS - #ELLIPSIS) .. ELLIPSIS
  end
  return text
end

-- `git status --porcelain=v1 -z` records look like "XY path\0", and a rename
-- or copy adds the original path as a record of its own.
function M.parse_status(raw)
  local entries = {}
  local position = 1
  while true do
    local stop = raw:find(SEPARATOR, position, true)
    if not stop then
      break
    end
    local record = raw:sub(position, stop - 1)
    position = stop + 1
    if #record > 3 then
      local code = record:sub(1, 2)
      entries[#entries + 1] = { path = record:sub(4), code = code }
      if code:sub(1, 1) == "R" or code:sub(1, 1) == "C" then
        local next_stop = raw:find(SEPARATOR, position, true)
        if not next_stop then
          break
        end
        position = next_stop + 1
      end
    end
  end
  return entries
end

local function style_for(line)
  if line == "" then
    return nil
  end
  if starts_with(line, ADDED_PREFIX) and not starts_with(line, "+++ ") then
    return "diff_new"
  end
  if starts_with(line, REMOVED_PREFIX) and not starts_with(line, "--- ") then
    return "diff_old"
  end
  if starts_with(line, HUNK_PREFIX) then
    return "tool_path"
  end
  for _, prefix in ipairs(FILE_HEADER_PREFIXES) do
    if starts_with(line, prefix) then
      return "dim"
    end
  end
  if starts_with(line, "\\") then
    return "dim"
  end
  return nil
end

local function lines_of(text)
  local lines = {}
  local position = 1
  while position <= #text do
    local stop = text:find("\n", position, true)
    if not stop then
      lines[#lines + 1] = text:sub(position)
      break
    end
    lines[#lines + 1] = text:sub(position, stop - 1)
    position = stop + 1
  end
  return lines
end

local function rows_of(text, max_rows, style)
  local rows = {}
  for _, line in ipairs(lines_of(text or "")) do
    if max_rows and #rows >= max_rows then
      rows[#rows + 1] = { text = ELLIPSIS .. " diff truncated", style = "dim" }
      break
    end
    rows[#rows + 1] = { text = line, style = style or style_for(line) }
  end
  return rows
end

function M.parse_diff(text, max_rows)
  return rows_of(text, max_rows)
end

-- An untracked file has no diff: every line it holds is new.
function M.untracked_rows(text, max_rows)
  return rows_of(text, max_rows, "diff_new")
end

function M.changed_label(count)
  if count == 0 then
    return "clean"
  end
  if count == 1 then
    return "1 change"
  end
  return count .. " changes"
end

-- The buffer, and the first and last row of the file list. A selected row is
-- one style for the whole line so it reads as the cursor, whatever the status.
function M.render(state, width)
  local lines = {}
  local header = state.error or state.summary or M.changed_label(#state.files)
  if state.branch and state.branch ~= "" then
    header = state.branch .. " · " .. header
  end
  lines[#lines + 1] = { { M.truncate(header, width), "bold" } }
  lines[#lines + 1] = { { "", "" } }
  local list_first = #lines + 1
  if #state.files == 0 then
    lines[#lines + 1] = { { " clean", "dim" } }
  else
    for index, file in ipairs(state.files) do
      local marker, style = M.kind(file.code)
      local row_style = index == state.selected and "item_selected" or style
      lines[#lines + 1] = { { M.truncate(" " .. marker .. " " .. file.path, width), row_style } }
    end
  end
  local list_last = #lines
  if #state.diff > 0 then
    lines[#lines + 1] = { { "", "" } }
    for _, row in ipairs(state.diff) do
      lines[#lines + 1] = { { M.truncate(row.text, width), row.style or "" } }
    end
  end
  return lines, list_first, list_last
end

return M
