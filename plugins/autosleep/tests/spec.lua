local helpers = require("autosleep_helpers")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

case("is_fresh_accepts_both_directions_of_skew", function()
  eq(helpers.is_fresh(100, 110, 15), true)
  eq(helpers.is_fresh(100, 90, 15), true)
  eq(helpers.is_fresh(100, 120, 15), false)
  eq(helpers.is_fresh(100, 80, 15), false)
end)

case("advance_starts_quiet_count_only_when_free", function()
  local quiet, fire = helpers.advance(nil, true, 100, 120)
  eq(quiet, nil)
  eq(fire, false)
  quiet, fire = helpers.advance(nil, false, 100, 120)
  eq(quiet, 100)
  eq(fire, false)
end)

case("advance_resets_quiet_count_when_work_resumes", function()
  local quiet, fire = helpers.advance(100, true, 150, 120)
  eq(quiet, nil)
  eq(fire, false)
end)

case("advance_fires_once_idle_period_elapsed", function()
  local quiet, fire = helpers.advance(100, false, 219, 120)
  eq(quiet, 100)
  eq(fire, false)
  quiet, fire = helpers.advance(100, false, 220, 120)
  eq(fire, true)
end)

th.report()
