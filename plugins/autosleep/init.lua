-- Sleeps the machine once every open makima instance has finished working.
-- An instance writes a heartbeat file while any of its sessions reports
-- "working", so the check holds across processes; a session waiting for input
-- counts as done. A missing, stale, or far-future heartbeat reads as free.
-- Sleeping is one-shot: run /autosleep to arm it again.
-- shortcut: only instances with this plugin enabled publish heartbeats, so an
-- instance without it stays invisible; enable it everywhere makima may run.

local helpers = require("autosleep_helpers")

local TICK_SECS = 5
-- A heartbeat older than this belongs to an instance that died mid-turn.
local STALE_SECS = 15
-- A gap this long between ticks means the machine slept on its own; restart
-- the quiet count so waking does not sleep again right away.
local RESUME_GAP_SECS = 60
local DIR_NAME = "autosleep"
local HEARTBEAT = "working"

local opts = maki.api.register_options({
  idle_secs = {
    default = 120,
    min = 5,
    desc = "Seconds every makima instance must stay idle before the machine sleeps.",
  },
})

local seed = maki.time.now()
local token = seed.secs .. "-" .. seed.nanosecs
local state_dir = maki.env.state_dir()
local dir = state_dir and maki.fs.joinpath(state_dir, DIR_NAME)
local heartbeat = dir and maki.fs.joinpath(dir, token)
if dir then
  maki.fs.mkdir(dir, { parents = true })
end

local armed = false
local quiet_since = nil
local last_tick = nil

local function now_secs()
  local t = maki.time.now()
  return t.secs + t.nanosecs / 1e9
end

-- No UI (print, ACP) reports no sessions, so a headless run never holds the
-- sleep back.
local function local_working()
  for _, session in ipairs(maki.session.live() or {}) do
    if session.status == "working" then
      return true
    end
  end
  return false
end

local function publish(working)
  if not heartbeat then
    return
  end
  if working then
    local _, err = maki.fs.write(heartbeat, HEARTBEAT)
    if err then
      maki.log.warn("autosleep: cannot publish heartbeat: " .. tostring(err))
    end
  else
    maki.fs.rm(heartbeat, { force = true })
  end
end

local function others_working(now)
  for _, entry in ipairs(maki.fs.dir(dir) or {}) do
    local name = entry[1]
    if name ~= token then
      local meta = maki.fs.metadata(maki.fs.joinpath(dir, name))
      if meta and meta.mtime and helpers.is_fresh(meta.mtime, now, STALE_SECS) then
        return true
      end
      maki.fs.rm(maki.fs.joinpath(dir, name), { force = true })
    end
  end
  return false
end

local function suspend_command()
  if maki.uv.os_getenv("OS") == "Windows_NT" then
    -- shortcut: hibernates while hibernation is enabled; `powercfg -h off`
    -- turns this into a plain sleep.
    return { "rundll32.exe", "powrprof.dll,SetSuspendState", "0,1,0" }
  end
  if maki.fn.executable("pmset") == 1 then
    return { "pmset", "sleepnow" }
  end
  return { "systemctl", "suspend" }
end

-- A command that cannot start or exits non-zero leaves the machine awake, so
-- warn and stay disarmed instead of retrying every quiet period.
local function suspend()
  local ok, err = pcall(maki.fn.jobstart, suspend_command(), {
    owner = "plugin",
    on_exit = function(_, code)
      if code ~= 0 then
        maki.log.warn("autosleep: suspend command exited with " .. code)
      end
    end,
  })
  if not ok then
    maki.log.warn("autosleep: cannot run the suspend command: " .. tostring(err))
  end
end

local function tick()
  local now = now_secs()
  local working = local_working()
  publish(working)
  if last_tick and now - last_tick > RESUME_GAP_SECS then
    quiet_since = nil
  end
  last_tick = now
  if not armed or not dir then
    return
  end
  local fire
  quiet_since, fire = helpers.advance(quiet_since, working or others_working(now), now, opts.idle_secs)
  if fire then
    armed = false
    quiet_since = nil
    suspend()
  end
end

maki.timer.set(TICK_SECS, tick)

maki.api.register_command({
  name = "/autosleep",
  description = "Sleep the machine once every makima instance finishes working",
  tui_only = true,
  handler = function()
    if not dir then
      maki.ui.flash("autosleep: state dir unavailable, cannot coordinate instances")
      return
    end
    armed = not armed
    quiet_since = nil
    if armed then
      maki.ui.flash("autosleep: sleeping after " .. opts.idle_secs .. "s of idle")
    else
      maki.ui.flash("autosleep: disarmed")
    end
  end,
})
