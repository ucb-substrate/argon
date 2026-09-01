---@mod argon.focus Focus handoff between Neovim and the Argon GUI.

local M = {}

local client = require('argon.client')

local gui_command_group = vim.api.nvim_create_augroup('argon_gui_command', { clear = true })

---Focus the Argon GUI.
function M.gui()
  client.any_buf_request('custom/startGui', nil, client.print_error)
end

local function return_to_gui_after_command()
  vim.api.nvim_create_autocmd('CmdlineEnter', {
    group = gui_command_group,
    pattern = ':',
    once = true,
    callback = function()
      vim.api.nvim_create_autocmd('CmdlineLeave', {
        group = gui_command_group,
        pattern = ':',
        once = true,
        callback = function()
          vim.schedule(M.gui)
        end,
      })
    end,
  })
end

---Focus Neovim and open a command line on behalf of the GUI.
---@param command? string
---@param opts? { return_to_gui?: boolean }
function M.editor(command, opts)
  opts = opts or {}
  local mode = vim.api.nvim_get_mode().mode
  local keys = mode:sub(1, 1) == 't' and '<C-\\><C-N>:' or '<Esc>:'
  if type(command) == 'string' then
    keys = keys .. command
  end

  -- Wait for this command line to be entered before watching it leave. The
  -- leading <Esc> may itself leave a pre-existing command line or prompt.
  vim.api.nvim_clear_autocmds({ group = gui_command_group })
  if opts.return_to_gui ~= false then
    return_to_gui_after_command()
  end
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), 'n', false)
end

return M
