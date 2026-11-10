-- CLIProxyAPI: a local gateway in front of subscriptions you already have.
-- Its key is optional (a deployment can turn auth off) and its address is a
-- port, so either a key or a configured URL is enough. Discovery asks for a
-- client version, the only catalog shape that carries context windows,
-- modalities and reasoning levels.

local compat = require("providers.openai_compat")
local chat = require("providers.chat_completions")
local http = require("providers.http")
local thinking = require("providers.thinking")

local M = {}

M.ID = "cliproxyapi"
M.DISPLAY_NAME = "CLIProxyAPI"
M.ENV = "CLIPROXYAPI_API_KEY"
M.BASE_URL_ENV = "CLIPROXYAPI_BASE_URL"
M.DEFAULT_BASE_URL = "http://127.0.0.1:8317/v1"
-- Any value works here: asking for a client version is what switches
-- `/v1/models` to the rich catalog.
M.CLIENT_VERSION = "maki"
M.API_SUFFIX = "/v1"
M.CHAT_PATH = "/chat/completions"
M.MODELS_PATH = "/models"
M.IMAGE = "image"
M.BASE_URL_KEY = "base_url"
M.HOST_PROMPT = "CLIProxyAPI URL"
M.MAX_TOKENS_FIELD = "max_tokens"
M.MISSING_CONFIG = "%s not set and no URL configured for '%s'; start CLIProxyAPI and run `makima auth login %s`"
M.NOTES = [[
CLIProxyAPI is a local gateway in front of subscriptions you already have. Makima looks for it on its default port, so all you need is `makima auth login cliproxyapi`. Answer the URL prompt when the gateway listens elsewhere, or set `CLIPROXYAPI_BASE_URL` instead.

A key is only needed when the gateway asks for one: set `CLIPROXYAPI_API_KEY`, or leave the login prompt empty. Models, context windows, image support and reasoning levels all come from the gateway's own listing.]]

local function text(value)
  if type(value) == "string" and value:match("%S") then
    return value
  end
end

-- Where the gateway listens: `base_url` in `providers.toml`, then
-- `CLIPROXYAPI_BASE_URL`. Nil leaves the default port.
function M.configured_url(config)
  return text(config.base_url) or text(maki.uv.os_getenv(M.BASE_URL_ENV))
end

-- The port is the address CPA users know, but its API lives under `/v1`, so
-- a bare origin would otherwise 404 on every request.
function M.api_base(raw)
  local trimmed = raw:match("^%s*(.-)%s*$"):gsub("/+$", "")
  if trimmed:sub(-#M.API_SUFFIX) == M.API_SUFFIX then
    return trimmed
  end
  return trimmed .. M.API_SUFFIX
end

-- Base URL, optional key pool and headers for {config}. A key or a configured
-- URL is required, so a gateway nobody configured is never probed on its
-- default port.
function M.client(ctx, config)
  local keys = http.api_keys(ctx, config, M.ENV)
  local root = M.configured_url(config)
  if not keys and not root then
    return nil, http.config_error(M.MISSING_CONFIG:format(M.ENV, M.ID, M.ID))
  end
  local configured, err = http.headers(M.ID, {}, config.headers)
  if not configured then
    return nil, err
  end
  local client = { base = M.api_base(root or M.DEFAULT_BASE_URL), pool = keys and http.key_pool(keys) }
  function client.headers(extra)
    local headers = table.clone(extra or {})
    if client.pool then
      headers.Authorization = "Bearer " .. client.pool.current()
    end
    for name, value in pairs(configured) do
      http.set_header(headers, name, value)
    end
    return headers
  end
  return client
end

-- A ladder entry is `{"effort": "high"}` or the bare string.
local function effort_name(level)
  local effort = type(level) == "table" and level.effort or level
  return type(effort) == "string" and effort or nil
end

-- A zero window means the gateway does not know it.
local function window(value)
  local limit = compat.limit(value)
  if limit and limit > 0 then
    return limit
  end
end

-- One rich catalog row as a descriptor. The ladder is kept so a request only
-- asks for a level the model lists; `ultra` and `none` have no Makima
-- equivalent and drop out of it.
function M.parse_model(row)
  local id = text(type(row.slug) == "string" and row.slug or row.id)
  if not id then
    return nil
  end
  local levels = {}
  for _, level in ipairs(type(row.supported_reasoning_levels) == "table" and row.supported_reasoning_levels or {}) do
    local name = effort_name(level)
    if name then
      levels[#levels + 1] = name
    end
  end
  local efforts = thinking.parse_levels(levels)
  -- An empty ladder is "the gateway does not say", not "no thinking": it
  -- reports nothing for the native Anthropic routes, which do reason.
  local supports_thinking
  if #levels > 0 then
    supports_thinking = false
    for _, name in ipairs(levels) do
      if name ~= thinking.OFF then
        supports_thinking = true
        break
      end
    end
  end
  local vision
  if type(row.input_modalities) == "table" then
    vision = table.find(row.input_modalities, M.IMAGE) ~= nil
  end
  return {
    id = id,
    context_window = window(row.context_window),
    supports_thinking = supports_thinking,
    supports_vision = vision,
    provider_data = #efforts > 0 and { efforts = efforts } or nil,
  }
end

-- Both catalog shapes: the rich `models` array, and the plain OpenAI `data`
-- listing of a gateway that ignores the client version.
function M.parse_models(body)
  body = type(body) == "table" and body or {}
  local entries = type(body.models) == "table" and body.models
    or type(body.data) == "table" and body.data
    or {}
  local models = {}
  for _, row in ipairs(entries) do
    local model = type(row) == "table" and M.parse_model(row)
    if model then
      models[#models + 1] = model
    end
  end
  table.sort(models, function(a, b)
    return a.id < b.id
  end)
  return models
end

-- The ladder the gateway advertises for {model}, or the shared fallback for
-- one discovery never saw. Adaptive means "on at a sensible depth", so it
-- lands on `high` snapped into what the model accepts.
function M.dialect(model)
  local data = type(model.provider_data) == "table" and model.provider_data or {}
  local efforts = type(data.efforts) == "table" and data.efforts or {}
  if #efforts == 0 then
    return thinking.dialects.prefer_high
  end
  return { supported = efforts, adaptive = thinking.snap("high", efforts) }
end

function M.stream(client, model, ctx, request, emit)
  local body = chat.build_request(request, { max_tokens_field = M.MAX_TOKENS_FIELD, include_stream_usage = true })
  if model.supports_thinking then
    body.reasoning_effort = thinking.effort(request.thinking, M.dialect(model), thinking.max_budget(request.model))
  end
  return http.post_stream(ctx, client.base .. M.CHAT_PATH, client.headers(), body, chat.stream, emit)
end

function M.provider()
  local spec = {
    id = M.ID,
    display_name = M.DISPLAY_NAME,
    config = { { key = M.BASE_URL_KEY, prompt = M.HOST_PROMPT, default = M.DEFAULT_BASE_URL } },
    auth = compat.api_key_auth({ display_name = M.DISPLAY_NAME, key_optional = true }),
    docs = { env = { M.ENV, M.BASE_URL_ENV }, endpoint = M.DEFAULT_BASE_URL, notes = M.NOTES },
  }

  -- A configured URL is enough: a deployment can turn auth off the same way
  -- it can move off the default port.
  function spec.auth_status(ctx, config)
    local keys, source = http.api_keys(ctx, config, M.ENV)
    if keys then
      return { state = source }
    end
    return { state = M.configured_url(config) and "configured" or "missing" }
  end

  function spec.discover(ctx, config)
    local client = M.client(ctx, config)
    if not client then
      return {}
    end
    local url = client.base .. M.MODELS_PATH .. "?client_version=" .. M.CLIENT_VERSION
    local response, err = http.send(ctx, { url = url, headers = client.headers() })
    if not response then
      return nil, err
    end
    local body, read_err = http.read_json(response)
    if read_err then
      return nil, read_err
    end
    return M.parse_models(body)
  end

  function spec.prepare(ctx, model, config)
    local client, err = M.client(ctx, config)
    if not client then
      return nil, err
    end
    local instance = {
      stream = function(call_ctx, request, emit)
        return M.stream(client, model, call_ctx, request, emit)
      end,
    }
    if client.pool then
      instance.key_count = client.pool.count
      instance.rotate_key = function()
        return client.pool.rotate()
      end
    end
    return instance, model
  end

  return spec
end

return M
