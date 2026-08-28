---@mod argon.server_status Neovim presentation for Argon analyzer activity.

local M = {}

local progress_title = 'Argon compilation'
local progress_message_id = 'argon.compilation'
local spinner_frames = { '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏' }

---@type table<integer, table<string, true>>
local active_tokens = {}
---@type uv.uv_timer_t|nil
local spinner_timer
local spinner_frame = 0

local function token_key(token)
  return type(token) .. ':' .. tostring(token)
end

local function active_count()
  local count = 0
  for _, tokens in pairs(active_tokens) do
    for _ in pairs(tokens) do
      count = count + 1
    end
  end
  return count
end

local function stop_spinner()
  if not spinner_timer then
    return
  end
  spinner_timer:stop()
  spinner_timer:close()
  spinner_timer = nil
end

local function render_running()
  local count = active_count()
  if count == 0 then
    return
  end
  spinner_frame = spinner_frame % #spinner_frames + 1
  local suffix = count > 1 and (' (%d)'):format(count) or ''
  vim.api.nvim_echo({ { spinner_frames[spinner_frame] .. ' Compiling' .. suffix } }, false, {
    id = progress_message_id,
    kind = 'progress',
    source = 'argon',
    title = 'Argon',
    status = 'running',
  })
end

local function finish_spinner()
  stop_spinner()
  vim.api.nvim_echo({ { '✓ Compilation complete' } }, false, {
    id = progress_message_id,
    kind = 'progress',
    source = 'argon',
    title = 'Argon',
    status = 'success',
  })
end

local function start_spinner()
  render_running()
  if spinner_timer then
    return
  end
  spinner_timer = vim.uv.new_timer()
  if spinner_timer then
    spinner_timer:start(80, 80, vim.schedule_wrap(render_running))
  end
end

---Update the visible spinner from an LSP work-done progress notification.
---@param client_id integer
---@param params lsp.ProgressParams
function M.update(client_id, params)
  local value = params and params.value
  if type(value) ~= 'table' then
    return
  end
  local key = token_key(params.token)
  local tokens = active_tokens[client_id]
  if value.kind == 'begin' and value.title == progress_title then
    tokens = tokens or {}
    active_tokens[client_id] = tokens
    tokens[key] = true
    start_spinner()
  elseif tokens and tokens[key] and value.kind == 'end' then
    tokens[key] = nil
    if not next(tokens) then
      active_tokens[client_id] = nil
    end
    if active_count() == 0 then
      finish_spinner()
    else
      render_running()
    end
  end
end

---Forget all progress owned by an exited or manually stopped LSP client.
---@param client_id integer
function M.reset_client_state(client_id)
  if not active_tokens[client_id] then
    return
  end
  active_tokens[client_id] = nil
  if active_count() == 0 then
    finish_spinner()
  else
    render_running()
  end
end

local progress_group = vim.api.nvim_create_augroup('argon_server_status', { clear = true })
vim.api.nvim_create_autocmd('LspProgress', {
  group = progress_group,
  callback = function(args)
    local client_id = args.data and args.data.client_id
    local client = client_id and vim.lsp.get_client_by_id(client_id)
    if client and client.name == 'argon' then
      M.update(client_id, args.data.params)
    end
  end,
})

return M
