-- The working tree, on the right: which files changed, and the diff of the one
-- under the cursor. Refreshes after each tool call, so edits show up as the
-- agent makes them.

local H = require("changes_helpers")

local TICK_MS = 250
local GIT_TIMEOUT_MS = 5000
local REFRESH_AFTER_MS = 600
local PANEL_WIDTH = "40%"
local MAX_DIFF_ROWS = 3000
local NO_CWD = "Changes: cannot read the working directory"
local NOT_A_REPO = "not a git repository"

local MOVE_KEYS = {
  j = 1,
  down = 1,
  k = -1,
  up = -1,
  ["ctrl+d"] = 10,
  pagedown = 10,
  ["ctrl+u"] = -10,
  pageup = -10,
}

local panel

local function git(cwd, argv)
  local command = { "git", "-C", cwd }
  for _, arg in ipairs(argv) do
    command[#command + 1] = arg
  end
  local ok, id = pcall(makima.fn.jobstart, command, { owner = "plugin" })
  if not ok then
    return nil
  end
  local result = makima.fn.jobwait(id, GIT_TIMEOUT_MS)
  if not result then
    makima.fn.jobstop(id)
    return nil
  end
  if result.exit_code ~= 0 then
    return nil
  end
  return result.stdout or ""
end

local function branch_of(cwd)
  local text = git(cwd, { "rev-parse", "--abbrev-ref", "HEAD" })
  if not text then
    return ""
  end
  return (text:gsub("%s+$", ""))
end

local function read_untracked(path)
  local ok, text = pcall(makima.fs.read, path)
  if not ok or type(text) ~= "string" then
    return ""
  end
  return text
end

local function load_diff(state)
  local file = state.files[state.selected]
  if not file then
    state.diff = {}
    return
  end
  if file.code:sub(1, 2) == "??" then
    state.diff = H.untracked_rows(read_untracked(file.path), MAX_DIFF_ROWS)
    return
  end
  local text = git(state.cwd, { "diff", "HEAD", "--", file.path })
  if text == nil then
    text = git(state.cwd, { "diff", "--", file.path })
  end
  state.diff = H.parse_diff(text or "", MAX_DIFF_ROWS)
end

local function render(state)
  local lines, list_first, list_last = H.render(state, state.width)
  state.line_count = #lines
  state.list_first = list_first
  state.list_last = list_last
  state.buf:set_lines(lines)
  state.win:set_cursor(state.cursor)
end

local function select(state, index)
  if index < 1 or index > #state.files or index == state.selected then
    return
  end
  state.selected = index
  load_diff(state)
  render(state)
  state.cursor = state.list_first + index - 1
  state.win:set_cursor(state.cursor)
end

local function refresh(state)
  state.dirty = false
  state.last_refresh = os.clock()
  local status = git(state.cwd, { "status", "--porcelain=v1", "-z" })
  if status == nil then
    state.error = NOT_A_REPO
    state.files = {}
    state.diff = {}
    state.selected = 1
    state.cursor = 1
    render(state)
    return
  end
  local previous = state.files[state.selected] and state.files[state.selected].path
  state.error = nil
  state.branch = branch_of(state.cwd)
  state.files = H.parse_status(status)
  state.selected = 1
  for index, file in ipairs(state.files) do
    if file.path == previous then
      state.selected = index
      break
    end
  end
  load_diff(state)
  render(state)
  state.cursor = state.list_first + state.selected - 1
  state.win:set_cursor(state.cursor)
end

local function move_to(state, row)
  state.cursor = math.max(1, math.min(row, state.line_count))
  if
    state.cursor >= state.list_first
    and state.cursor <= state.list_last
    and #state.files > 0
  then
    select(state, state.cursor - state.list_first + 1)
  else
    state.win:set_cursor(state.cursor)
  end
end

local function close(state)
  panel = nil
  state.win:close()
end

local function handle_key(state, key)
  if key == "esc" or key == "q" or key == "ctrl+c" then
    close(state)
    return
  end
  if key == "r" then
    refresh(state)
    return
  end
  if key == "g" or key == "home" then
    move_to(state, 1)
    return
  end
  if key == "G" or key == "end" then
    move_to(state, state.line_count)
    return
  end
  local step = MOVE_KEYS[key]
  if step then
    move_to(state, state.cursor + step)
  end
end

local function open()
  local cwd = makima.uv.cwd()
  if not cwd then
    makima.ui.flash(NO_CWD)
    return
  end
  local buf = makima.ui.buf()
  local win = makima.ui.open_win(buf, {
    split = "right",
    width = PANEL_WIDTH,
    title = " Changes ",
    border = "rounded",
    focus = true,
    cursor_line = true,
    footer = { { "j/k", "file" }, { "r", "refresh" }, { "esc", "close" } },
  })
  local state = {
    buf = buf,
    win = win,
    cwd = cwd,
    width = win.width or 40,
    files = {},
    diff = {},
    selected = 1,
    cursor = 1,
    line_count = 1,
  }
  panel = state
  refresh(state)
  while panel == state do
    local event = win:recv(TICK_MS)
    if not event or event.type == "close" then
      close(state)
    elseif event.type == "resize" then
      state.width = event.width
      render(state)
    elseif event.type == "key" then
      handle_key(state, event.key)
    elseif event.type == "timeout" and state.dirty then
      refresh(state)
    end
  end
end

local function toggle()
  if panel then
    close(panel)
  else
    open()
  end
end

makima.api.create_autocmd("ToolDone", {
  callback = function()
    if not panel then
      return
    end
    if os.clock() - (panel.last_refresh or 0) >= REFRESH_AFTER_MS / 1000 then
      panel.dirty = true
    end
  end,
})

makima.api.register_command({
  name = "/changes",
  description = "Show the working tree changes and their diff",
  tui_only = true,
  handler = toggle,
})

makima.keymap.set("n", "<C-g>", toggle, { desc = "Toggle changes panel" })
