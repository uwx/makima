-- Pure decision logic for autosleep.
local M = {}

-- A heartbeat counts as working only while it is fresh in both directions: an
-- old one belongs to a process that died mid-turn, a far-future one is a clock
-- jump.
function M.is_fresh(mtime, now, stale_secs)
  return math.abs(now - mtime) <= stale_secs
end

-- Returns the quiet-since timestamp (nil while someone works) and whether the
-- machine should sleep now.
function M.advance(quiet_since, working, now, idle_secs)
  if working then
    return nil, false
  end
  quiet_since = quiet_since or now
  return quiet_since, now - quiet_since >= idle_secs
end

return M
