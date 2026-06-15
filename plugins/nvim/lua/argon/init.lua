local M = {}

local client = require('argon.client')
local config = require('argon.config').config
local commands = require('argon.commands')
local uv = vim.uv or vim.loop

local function dirname(path)
  return vim.fn.fnamemodify(path, ':h')
end

local function find_workspace_root(path)
  local current = dirname(path)
  while current and current ~= '.' and current ~= '/' do
    if vim.fn.filereadable(current .. '/lib.ar') == 1 then
      return current
    end
    local parent = dirname(current)
    if parent == current then
      break
    end
    current = parent
  end
end

---LSP restart internal implementations
---@param bufnr? number The buffer number, defaults to the current buffer
---@param filter? vim.lsp.get_clients.Filter
---@param callback? fun(client: vim.lsp.Client) Optional callback to run for each client before restarting.
---@return number|nil client_id
local function restart(bufnr, filter, callback)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local clients = M.stop(bufnr, filter)
  local timer = uv and uv.new_timer and uv.new_timer() or nil
  if not timer then
    vim.schedule(function()
      vim.notify('argon: Failed to initialise timer for LSP client restart.', vim.log.levels.ERROR)
    end)
    return
  end
  local max_attempts = 50
  local attempts_to_live = max_attempts
  local stopped_client_count = 0
  timer:start(200, 100, function()
    for _, client in ipairs(clients) do
      if client:is_stopped() then
        stopped_client_count = stopped_client_count + 1
        vim.schedule(function()
          -- Execute the callback, if provided, for additional actions before restarting
          if callback then
            callback(client)
          end
          M.start(bufnr)
        end)
      end
    end
    if stopped_client_count >= #clients then
      timer:stop()
      attempts_to_live = 0
    elseif attempts_to_live <= 0 then
      vim.schedule(function()
        vim.notify(
          ('argon: Could not restart all LSP clients after %d attempts.'):format(max_attempts),
          vim.log.levels.ERROR
        )
      end)
      timer:stop()
      attempts_to_live = 0
    end
    attempts_to_live = attempts_to_live - 1
  end)
end

M.get_root_dir = function(bufnr)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local bufname = vim.api.nvim_buf_get_name(bufnr)
  return find_workspace_root(bufname)
end

--- Start or attach the LSP client
---@param bufnr? number The buffer number (optional), defaults to the current buffer
M.start = function(bufnr)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local bufname = vim.api.nvim_buf_get_name(bufnr)
  local root_dir = M.get_root_dir(bufnr)
  if not root_dir then
    vim.notify(
      'argon: Could not detect workspace, treating current file as root.',
      vim.log.levels.WARN
    )
    root_dir = dirname(bufname)
  end
  local cmd_env = {}
  if config.log.level then
    cmd_env.ARGON_LOG = config.log.level
  end
  local lang_server = config.argon_repo_path and (config.argon_repo_path .. '/target/release/lang-server')
    or 'lang-server'
  local lsp_start_config = {
    name = 'argon',
    cmd = { lang_server },
    cmd_env = cmd_env,
    handlers = {
      ['custom/forceSave'] = function(_, result, _)
        local target_bufnr = vim.fn.bufnr(result)

        if target_bufnr ~= -1 then
          vim.api.nvim_buf_call(target_bufnr, function()
            vim.cmd('write')
          end)
        end

        return vim.NIL
      end,
      ['custom/undo'] = function(_, _, _)
        local current_bufnr = vim.api.nvim_get_current_buf()

        if current_bufnr ~= -1 then
          vim.api.nvim_buf_call(current_bufnr, function()
            vim.cmd('undo')
            vim.cmd('write')
          end)
        end

        return vim.NIL
      end,
      ['custom/redo'] = function(_, _, _)
        local current_bufnr = vim.api.nvim_get_current_buf()

        if current_bufnr ~= -1 then
          vim.api.nvim_buf_call(current_bufnr, function()
            vim.cmd('redo')
            vim.cmd('write')
          end)
        end

        return vim.NIL
      end,
    },
    root_dir = root_dir,
  }

  local old_on_init = lsp_start_config.on_init
  lsp_start_config.on_init = function(...)
    commands.create_argon_command()
    if type(old_on_init) == 'function' then
      old_on_init(...)
    end
  end

  local old_on_exit = lsp_start_config.on_exit
  lsp_start_config.on_exit = function(...)
    vim.schedule(function()
      commands.delete_argon_command()
    end)
    if type(old_on_exit) == 'function' then
      old_on_exit(...)
    end
  end

  vim.lsp.start(lsp_start_config, { bufnr = bufnr })
end

---Stop the LSP client.
---@param bufnr? number The buffer number, defaults to the current buffer
---@param filter? vim.lsp.get_clients.Filter
---@return vim.lsp.Client[] clients A list of clients that will be stopped
M.stop = function(bufnr, filter)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local clients = client.get_active_argon_lsp_clients(bufnr, filter)
  vim.lsp.stop_client(clients)
  return clients
end

---Restart the LSP client.
---Fails silently if the buffer's filetype is not one of the filetypes specified in the config.
---@return number|nil client_id The LSP client ID after restart
M.restart = function()
  return restart()
end

return M
