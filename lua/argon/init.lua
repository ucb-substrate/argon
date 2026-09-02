local M = {}

local client = require('argon.client')
local config = require('argon.config').config
local commands = require('argon.commands')
local focus = require('argon.focus')
local save = require('argon.save')
local server_status = require('argon.server_status')

local function schedule_workspace_modified(client_id)
    vim.schedule(function()
        save.notify_workspace_modified(client_id)
    end)
end

local modified_clients_by_buffer = {}

local function schedule_buffer_clients(bufnr)
    for client_id in pairs(modified_clients_by_buffer[bufnr] or {}) do
        schedule_workspace_modified(client_id)
    end
end

local function track_modified_buffer(bufnr, client_id)
    local clients = modified_clients_by_buffer[bufnr]
    if clients then
        clients[client_id] = true
        return
    end
    modified_clients_by_buffer[bufnr] = { [client_id] = true }
    vim.api.nvim_buf_attach(bufnr, false, {
        on_lines = function()
            schedule_buffer_clients(bufnr)
        end,
        on_detach = function()
            schedule_buffer_clients(bufnr)
            modified_clients_by_buffer[bufnr] = nil
        end,
    })
end

--- Buffers this plugin installed its own `gd` mapping in.
local gd_mapped_buffers = {}

--- Drops the `gd` mapping this plugin installed, once no argon client is left
--- attached.
---
--- `vim.lsp.buf.definition` does nothing without a client, so leaving it would
--- permanently shadow Vim's builtin `gd` for the life of the buffer -- and
--- make the next attach believe someone else already owns the mapping.
---@param bufnr number
local function release_gd_mapping(bufnr)
  if not gd_mapped_buffers[bufnr] then
    return
  end
  if #vim.lsp.get_clients({ name = 'argon', bufnr = bufnr }) > 0 then
    return
  end
  gd_mapped_buffers[bufnr] = nil
  if vim.api.nvim_buf_is_valid(bufnr) then
    pcall(vim.keymap.del, 'n', 'gd', { buffer = bufnr })
  end
end

local modified_group = vim.api.nvim_create_augroup('argon_workspace_modified', { clear = true })
vim.api.nvim_create_autocmd('BufModifiedSet', {
    group = modified_group,
    callback = function(args)
        schedule_buffer_clients(args.buf)
    end,
})
vim.api.nvim_create_autocmd('BufWritePost', {
    group = modified_group,
    callback = function(args)
        schedule_buffer_clients(args.buf)
    end,
})
vim.api.nvim_create_autocmd('LspAttach', {
    group = modified_group,
    callback = function(args)
        local attached_client = vim.lsp.get_client_by_id(args.data.client_id)
        if not attached_client or attached_client.name ~= 'argon' then
            return
        end
        track_modified_buffer(args.buf, args.data.client_id)
        schedule_workspace_modified(args.data.client_id)
    end,
})
vim.api.nvim_create_autocmd('LspDetach', {
    group = modified_group,
    callback = function(args)
        local clients = modified_clients_by_buffer[args.buf]
        if clients then
            clients[args.data.client_id] = nil
        end
        schedule_workspace_modified(args.data.client_id)
        -- Scheduled: the detaching client is still attached at this point.
        vim.schedule(function()
            release_gd_mapping(args.buf)
        end)
    end,
})

for _, lhs in ipairs({ '<C-\\>', string.char(28) }) do
    vim.keymap.set({ 'n', 'i', 'v', 'c', 't' }, lhs, focus.gui, {
        desc = 'Focus Argon GUI',
        silent = true,
        nowait = true,
    })
end

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

--- Whether `gd` is already mapped for `bufnr`, buffer-locally or globally.
---
--- `maparg` would answer for the *current* buffer, which is not necessarily
--- the one being attached to, and would report a buffer-local mapping in
--- preference to a global one. Both lists are read directly instead.
---@param bufnr number The buffer to check
---@return boolean
local function gd_is_mapped(bufnr)
  for _, mappings in ipairs({
    vim.api.nvim_buf_get_keymap(bufnr, 'n'),
    vim.api.nvim_get_keymap('n'),
  }) do
    for _, mapping in ipairs(mappings) do
      if mapping.lhs == 'gd' then
        return true
      end
    end
  end
  return false
end

--- The directory the analyzer writes the embedded standard library into.
---
--- Mirrors `argon_cache_dir` in the analyzer: `$XDG_CACHE_HOME`, or `~/.cache`
--- when it is unset.
---@return string|nil
local function std_cache_dir()
  local base = vim.env.XDG_CACHE_HOME
  if not base or base == '' then
    local home = vim.uv.os_homedir() or vim.env.HOME
    if not home or home == '' then
      return nil
    end
    base = home .. '/.cache'
  end
  return vim.fs.normalize(base .. '/argon/std')
end

M.get_root_dir = function(bufnr)
    bufnr = bufnr or vim.api.nvim_get_current_buf()
    local bufname = vim.api.nvim_buf_get_name(bufnr)
    local lib_dir = vim.fs.root(bufname, { 'lib.ar' })
    return lib_dir
end

--- Start or attach the LSP client
---@param bufnr? number The buffer number (optional), defaults to the current buffer
M.start = function(bufnr)
    bufnr = bufnr or vim.api.nvim_get_current_buf()
    -- A buffer under the cache directory is the read-only copy of the standard
    -- library that go-to-definition opened. Its root file is named `lib.ar`,
    -- so workspace detection would treat it as a project of its own and start
    -- a second analyzer rooted at the cache; the client the user jumped from
    -- already answers for it.
    local std_cache = std_cache_dir()
    if
        std_cache
        and vim.startswith(
            vim.fs.normalize(vim.api.nvim_buf_get_name(bufnr)),
            std_cache .. '/'
        )
    then
        return
    end
    if not config.cmd and vim.fn.executable(config.analyzer) ~= 1 then
        vim.notify(
          'argon: Could not find argon-analyzer. Install it with Cargo or configure vim.g.argon.analyzer.',
          vim.log.levels.ERROR
        )
        return
    end
    local root_dir = M.get_root_dir(bufnr)
    if not root_dir then
        vim.notify(
          'argon: Could not detect workspace, treating current file as root.',
          vim.log.levels.WARN
        )
        root_dir = vim.fs.dirname(vim.api.nvim_buf_get_name(bufnr))
    end
    local analyzer_cmd = config.cmd or { config.analyzer }
    if type(analyzer_cmd) == 'table' then
      analyzer_cmd = vim.deepcopy(analyzer_cmd)
    end
    if type(analyzer_cmd) == 'table' and vim.g.argon_analyzer_rpc_port then
        vim.list_extend(analyzer_cmd, {
            '--rpc-port',
            tostring(vim.g.argon_analyzer_rpc_port),
        })
    end
    if type(analyzer_cmd) == 'table' and vim.g.argon_analyzer_relay then
        vim.list_extend(analyzer_cmd, {
            '--relay-socket',
            tostring(vim.g.argon_analyzer_relay),
        })
    end
    local lsp_start_config = { 
        name = 'argon',
        cmd = analyzer_cmd,
        handlers = {
            ['custom/save'] = function(err, result, ctx)
                if err then
                    client.print_error(err)
                    return vim.NIL
                end

                vim.schedule(function()
                    save.save_modified_buffers(ctx.client_id)
                end)
                return vim.NIL
            end,
            ['custom/undo'] = function(err, result, ctx)
                local bufnr = vim.api.nvim_get_current_buf()

                if bufnr ~= -1 then
                    vim.api.nvim_buf_call(bufnr, function()
                        vim.cmd('undo')
                    end)
                end

                return vim.NIL
            end,
            ['custom/redo'] = function(err, result, ctx)
                local bufnr = vim.api.nvim_get_current_buf()

                if bufnr ~= -1 then
                    vim.api.nvim_buf_call(bufnr, function()
                        vim.cmd('redo')
                    end)
                end

                return vim.NIL
            end,
            ['custom/focusEditor'] = function(_, params, _)
                vim.schedule(function()
                    focus.editor(params.command, { return_to_gui = params.return_to_gui })
                end)
            end,
        },
        root_dir = root_dir
    }

    local old_on_init = lsp_start_config.on_init
    lsp_start_config.on_init = function(lsp_client, ...)
        commands.create_argon_command()
        if type(old_on_init) == 'function' then
            old_on_init(lsp_client, ...)
        end
        if vim.g.argon_auto_gui then
            vim.schedule(function()
                lsp_client:request('custom/startGui', nil, client.print_error, bufnr)
            end)
        end
        schedule_workspace_modified(lsp_client.id)
    end

    local old_on_attach = lsp_start_config.on_attach
    lsp_start_config.on_attach = function(lsp_client, attached_bufnr, ...)
        -- Neovim maps `grr` to references and points `tagfunc` (so `<C-]>`)
        -- at go-to-definition on its own. `gd` is the mapping people reach
        -- for first, so provide it, leaving any existing one alone.
        if
            lsp_client:supports_method('textDocument/definition')
            and not gd_is_mapped(attached_bufnr)
        then
            vim.keymap.set('n', 'gd', vim.lsp.buf.definition, {
                buffer = attached_bufnr,
                desc = 'argon: go to definition',
            })
            gd_mapped_buffers[attached_bufnr] = true
        end
        if type(old_on_attach) == 'function' then
            old_on_attach(lsp_client, attached_bufnr, ...)
        end
    end

    local old_on_exit = lsp_start_config.on_exit
    lsp_start_config.on_exit = function(code, signal, client_id, ...)
        -- on_exit runs in_fast_event
        vim.schedule(function()
          commands.delete_argon_command()
          server_status.reset_client_state(client_id)
        end)
        if type(old_on_exit) == 'function' then
          old_on_exit(code, signal, client_id, ...)
        end
    end

    local client_id = vim.lsp.start(lsp_start_config, { bufnr = bufnr })
    if client_id then
        track_modified_buffer(bufnr, client_id)
    end
    return client_id
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
