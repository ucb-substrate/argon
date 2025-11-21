local M = {}

local client = require('argon.client')
local config = require('argon.config').config
local commands = require('argon.commands')

---LSP restart internal implementations
---@param bufnr? number The buffer number, defaults to the current buffer
---@param filter? vim.lsp.get_clients.Filter
---@param callback? fun(client: vim.lsp.Client) Optional callback to run for each client before restarting.
---@return number|nil client_id
local function restart(bufnr, filter, callback)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local clients = M.stop(bufnr, filter)
  local timer, _, _ = vim.uv.new_timer()
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
    local crate_dir = vim.fs.root(bufname, { 'lib.ar' })
    return crate_dir
end

--- Start or attach the LSP client
---@param bufnr? number The buffer number (optional), defaults to the current buffer
M.start = function(bufnr)
    local root_dir = M.get_root_dir(bufnr)
    if not root_dir then
        vim.notify(
          'argon: Could not detect workspace, treating current file as root.',
          vim.log.levels.WARN
        )
        root_dir = vim.fs.dirname(bufname)
    end
    local cmd_env = {}
    if config.log.level then
        cmd_env.ARGON_LOG = config.log.level
    end
    local lsp_start_config = { 
        name = 'argon',
        cmd = { config.argon_repo_path ..'/target/release/lang-server' },
        cmd_env = cmd_env,
        handlers = {
            ['custom/forceSave'] = function(err, result, ctx)
                local bufnr = vim.fn.bufnr(result)

                if bufnr ~= -1 then
                    vim.api.nvim_buf_call(bufnr, function()
                        vim.cmd('write')
                    end)
                end

                return vim.NIL
            end,
            ['custom/undo'] = function(err, result, ctx)
                local bufnr = vim.api.nvim_get_current_buf()

                if bufnr ~= -1 then
                    vim.api.nvim_buf_call(bufnr, function()
                        vim.cmd('undo')
                        vim.cmd('write')
                    end)
                end

                return vim.NIL
            end,
            ['custom/redo'] = function(err, result, ctx)
                local bufnr = vim.api.nvim_get_current_buf()

                if bufnr ~= -1 then
                    vim.api.nvim_buf_call(bufnr, function()
                        vim.cmd('redo')
                        vim.cmd('write')
                    end)
                end

                return vim.NIL
            end,
        },
        root_dir = root_dir
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
        -- on_exit runs in_fast_event
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
---@return vim.lsp.Client[] clients A list of clients that will be stopped
M.stop = function(bufnr)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local clients = client.get_active_argon_lsp_clients(bufnr, filter)
  vim.lsp.stop_client(clients)
  if type(clients) == 'table' then
    ---@cast clients vim.lsp.Client[]
    for _, client in ipairs(clients) do
      server_status.reset_client_state(client.id)
    end
  else
    ---@cast clients vim.lsp.Client
    server_status.reset_client_state(clients.id)
  end
  return clients
end

---Restart the LSP client.
---Fails silently if the buffer's filetype is not one of the filetypes specified in the config.
---@return number|nil client_id The LSP client ID after restart
M.restart = function()
  return restart()
end

return M
