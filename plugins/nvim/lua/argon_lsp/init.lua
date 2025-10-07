local M = {}

local client = require('argon_lsp.client')
local config = require('argon_lsp.config')
local commands = require('argon_lsp.commands')

---LSP restart internal implementations
---@param bufnr? number The buffer number, defaults to the current buffer
---@param filter? rustaceanvim.lsp.get_clients.Filter
---@param callback? fun(client: vim.lsp.Client) Optional callback to run for each client before restarting.
---@return number|nil client_id
local function restart(bufnr, filter, callback)
  bufnr = bufnr or vim.api.nvim_get_current_buf()
  local clients = M.stop(bufnr, filter)
  local timer, _, _ = vim.uv.new_timer()
  if not timer then
    vim.schedule(function()
      vim.notify('argon_lsp: Failed to initialise timer for LSP client restart.', vim.log.levels.ERROR)
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
          ('argon_lsp: Could not restart all LSP clients after %d attempts.'):format(max_attempts),
          vim.log.levels.ERROR
        )
      end)
      timer:stop()
      attempts_to_live = 0
    end
    attempts_to_live = attempts_to_live - 1
  end)
end

--- Start or attach the LSP client
---@param bufnr? number The buffer number (optional), defaults to the current buffer
M.start = function(bufnr)
    local lsp_start_config = { 
        name = 'argon_lsp',
        cmd = { config.argon_repo_path ..'/target/debug/lsp-server' },
        handlers = {
            ['custom/forceSave'] = function(err, result, ctx)
                print("Handler called!", vim.inspect(command))
                --- TODO: write to correct buffer.
                vim.cmd('write')
                return vim.NIL
            end
        }
    }
    
    bufnr = bufnr or vim.api.nvim_get_current_buf()
    local bufname = vim.api.nvim_buf_get_name(bufnr)
    root_dir = vim.fs.dirname(bufname)
    lsp_start_config.root_dir = root_dir

    local old_on_init = lsp_start_config.on_init
    lsp_start_config.on_init = function(...)
        commands.create_argon_lsp_command()
        if type(old_on_init) == 'function' then
            old_on_init(...)
        end
    end

    local old_on_exit = lsp_start_config.on_exit
    lsp_start_config.on_exit = function(...)
        -- on_exit runs in_fast_event
        vim.schedule(function()
        commands.delete_argon_lsp_command()
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
