local cliproxyapi = require("providers.cliproxyapi")
local http = require("providers.http")
local t = require("makima.test_helpers")

local ROOT = "http://127.0.0.1:8317"
local BASE = ROOT .. "/v1"
local MODEL_ID = "glm-5.3-flash"
local DONE = "data: [DONE]\n\n"
local RICH = {
  models = {
    {
      slug = MODEL_ID,
      context_window = 500000,
      input_modalities = { "text", "image" },
      supported_reasoning_levels = {
        { effort = "low" },
        { effort = "medium" },
        { effort = "high" },
        { effort = "max" },
        { effort = "ultra" },
      },
    },
  },
}

local spec = cliproxyapi.provider()

local function sse()
  return t.response({ 'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\n' .. DONE })
end

local function json(value)
  return t.response({ assert(makima.json.encode(value)) })
end

local function request(thinking)
  return {
    model = { id = MODEL_ID, max_output_tokens = 16384, context_window = 500000 },
    system = "sys",
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
    tools = {},
    thinking = thinking or { kind = "off" },
  }
end

local function prepare(env, config, responses, saved, model)
  local ctx = t.provider_ctx(responses or { sse() }, saved)
  local instance, described
  t.with_env(env, function()
    instance, described = spec.prepare(ctx, model or { id = MODEL_ID }, config or {})
  end)
  return ctx, instance, described
end

local function sent(env, config, thinking, model, saved)
  local ctx, instance = prepare(env, config, nil, saved, model)
  assert(instance, "prepare failed")
  assert(instance.stream(ctx, request(thinking), t.recorder()))
  return ctx.requests[1]
end

t.case("api_base_appends_v1_once", function()
  t.eq(cliproxyapi.api_base(ROOT), BASE)
  t.eq(cliproxyapi.api_base(ROOT .. "/"), BASE)
  t.eq(cliproxyapi.api_base(BASE), BASE)
end)

t.case("rich_catalog_row_becomes_a_descriptor", function()
  local model = cliproxyapi.parse_model(RICH.models[1])
  t.eq(model.id, MODEL_ID)
  t.eq(model.context_window, 500000)
  t.eq(model.supports_vision, true)
  t.eq(model.supports_thinking, true)
  t.same(model.provider_data.efforts, { "low", "medium", "high", "max" })
end)

t.case("plain_catalog_row_keeps_unknowns", function()
  local models = cliproxyapi.parse_models({ data = { { id = MODEL_ID, object = "model" } } })
  t.eq(#models, 1)
  t.eq(models[1].id, MODEL_ID)
  t.eq(models[1].context_window, nil)
  t.eq(models[1].supports_thinking, nil)
  t.eq(models[1].supports_vision, nil)
end)

t.case("thinking_follows_the_reasoning_ladder", function()
  local function described(levels)
    return cliproxyapi.parse_model({ slug = MODEL_ID, supported_reasoning_levels = levels })
  end
  t.eq(described({}).supports_thinking, nil)
  t.eq(described({ { effort = "none" } }).supports_thinking, false)
  t.eq(described({ { effort = "ultra" } }).supports_thinking, true)
  t.eq(described({ { effort = "none" }, { effort = "high" } }).supports_thinking, true)
  t.eq(described({ "high" }).supports_thinking, true)
end)

t.case("zero_context_window_is_unknown", function()
  t.eq(cliproxyapi.parse_model({ slug = MODEL_ID, context_window = 0 }).context_window, nil)
end)

t.case("discovery_asks_for_a_client_version", function()
  local ctx = t.provider_ctx({ json(RICH) })
  local models
  t.with_env({ CLIPROXYAPI_API_KEY = "k" }, function()
    models = spec.discover(ctx, {})
  end)
  t.eq(ctx.requests[1].url, BASE .. "/models?client_version=makima")
  t.eq(ctx.requests[1].headers.Authorization, "Bearer k")
  t.eq(#models, 1)
  t.eq(models[1].id, MODEL_ID)
end)

t.case("discovery_reads_the_plain_shape_and_sorts", function()
  local ctx = t.provider_ctx({ json({ data = { { id = "b" }, { id = "a" } } }) })
  local models
  t.with_env({}, function()
    models = spec.discover(ctx, { base_url = ROOT })
  end)
  t.eq(ctx.requests[1].url, BASE .. "/models?client_version=makima")
  t.eq(models[1].id, "a")
  t.eq(models[2].id, "b")
end)

t.case("a_key_or_a_url_is_required", function()
  t.with_env({}, function()
    t.same(spec.discover(t.provider_ctx({}), {}), {})
    local instance, err = spec.prepare(t.provider_ctx({}), { id = MODEL_ID }, {})
    t.eq(instance, nil)
    t.eq(err.kind, http.CONFIG_KIND)
    t.eq(err.message, cliproxyapi.MISSING_CONFIG:format(cliproxyapi.ENV, cliproxyapi.ID, cliproxyapi.ID))
  end)
end)

t.case("a_configured_url_needs_no_key", function()
  local req = sent({ CLIPROXYAPI_BASE_URL = ROOT })
  t.eq(req.url, BASE .. "/chat/completions")
  t.eq(req.headers.Authorization, nil)
  local _, instance = prepare({ CLIPROXYAPI_BASE_URL = ROOT })
  t.eq(instance.key_count, nil)
  t.eq(instance.rotate_key, nil)
end)

t.case("keys_rotate", function()
  local ctx, instance = prepare({ CLIPROXYAPI_API_KEY = "a,b" })
  t.eq(instance.key_count, 2)
  assert(instance.rotate_key())
  assert(instance.stream(ctx, request(), t.recorder()))
  t.eq(ctx.requests[1].headers.Authorization, "Bearer b")
end)

t.case("the_advertised_ladder_caps_the_effort_sent", function()
  local model = cliproxyapi.parse_model(RICH.models[1])
  local function effort(thinking)
    return makima.json.decode(sent({ CLIPROXYAPI_API_KEY = "k" }, {}, thinking, model).body).reasoning_effort
  end
  t.eq(effort({ kind = "adaptive" }), "high")
  t.eq(effort({ kind = "effort", level = "max" }), "max")
  t.eq(effort({ kind = "off" }), nil)
  local low = cliproxyapi.parse_model({ slug = MODEL_ID, supported_reasoning_levels = { { effort = "low" } } })
  local body = makima.json.decode(sent({ CLIPROXYAPI_API_KEY = "k" }, {}, { kind = "adaptive" }, low).body)
  t.eq(body.reasoning_effort, "low")
end)

t.case("an_unknown_model_takes_the_shared_ladder", function()
  local model = { id = MODEL_ID, supports_thinking = true }
  local body = makima.json.decode(sent({ CLIPROXYAPI_API_KEY = "k" }, {}, { kind = "adaptive" }, model).body)
  t.eq(body.reasoning_effort, "high")
end)

t.case("thinking_is_not_sent_to_models_that_lack_it", function()
  local body = makima.json.decode(sent({ CLIPROXYAPI_API_KEY = "k" }, {}, { kind = "adaptive" }).body)
  t.eq(body.reasoning_effort, nil)
  t.eq(body.max_tokens, 16384)
  t.eq(body.stream_options.include_usage, true)
end)

t.case("auth_status_counts_a_configured_url", function()
  t.with_env({ CLIPROXYAPI_API_KEY = "k" }, function()
    t.eq(spec.auth_status(t.provider_ctx({}), {}).state, "env")
  end)
  t.with_env({ CLIPROXYAPI_BASE_URL = ROOT }, function()
    t.eq(spec.auth_status(t.provider_ctx({}), {}).state, "configured")
  end)
  t.with_env({}, function()
    t.eq(spec.auth_status(t.provider_ctx({}, { api_key = "saved" }), {}).state, "saved")
    t.eq(spec.auth_status(t.provider_ctx({}), { base_url = ROOT }).state, "configured")
    t.eq(spec.auth_status(t.provider_ctx({}), {}).state, "missing")
  end)
end)

t.report()
