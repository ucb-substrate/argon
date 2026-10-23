local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

local gui_focus_count = 0
package.loaded['argon.client'] = {
  any_buf_request = function(method, params)
    assert(method == 'custom/startGui')
    assert(params == nil)
    gui_focus_count = gui_focus_count + 1
  end,
  print_error = function() end,
}

local focus = require('argon.focus')

local stage = 1
local exit_keys = { '<CR>', '<CR>', '<Esc>', '<C-C>', '<Esc>', '<CR>' }
local pending_command = 'let g:argon_focus_pending = 1'
local pending_replaced = false

local function checked(callback)
  local ok, err = pcall(callback)
  if not ok then
    vim.api.nvim_err_writeln(tostring(err))
    vim.cmd('cquit')
  end
end

vim.api.nvim_create_autocmd('CmdlineEnter', {
  callback = function()
    -- The pending command line stays open until the GUI replaces it.
    if stage == 6 and not pending_replaced then
      return
    end
    local exit_key = assert(exit_keys[stage])
    vim.schedule(function()
      vim.api.nvim_input(exit_key)
    end)
  end,
})

vim.api.nvim_create_autocmd('CmdlineChanged', {
  callback = function()
    if stage == 6 and not pending_replaced and vim.fn.getcmdline() == pending_command then
      pending_replaced = true
      vim.schedule(function()
        focus.editor('let g:argon_focus_replaced = 1')
      end)
    end
  end,
})

vim.api.nvim_create_autocmd('CmdlineLeave', {
  callback = function()
    vim.defer_fn(function()
      checked(function()
        if stage == 1 then
          assert(vim.g.argon_focus_stayed_in_editor == 1)
          assert(gui_focus_count == 0, 'configured editor command should not focus the GUI')
          stage = 2
          focus.editor('let g:argon_focus_executed = 1')
        elseif stage == 2 then
          assert(vim.g.argon_focus_executed == 1)
          assert(gui_focus_count == 1, 'executing a GUI command should focus the GUI')
          stage = 3
          focus.editor('let g:argon_focus_cancelled = 1')
        elseif stage == 3 then
          assert(vim.g.argon_focus_cancelled == nil)
          assert(gui_focus_count == 2, 'cancelling a GUI command should focus the GUI')
          stage = 4
          focus.editor('let g:argon_focus_interrupted = 1')
        elseif stage == 4 then
          assert(vim.g.argon_focus_interrupted == nil)
          assert(gui_focus_count == 3, 'interrupting a GUI command should focus the GUI')
          stage = 5
          vim.api.nvim_input(':let g:argon_ordinary_command = 1')
        elseif stage == 5 then
          assert(vim.g.argon_ordinary_command == nil)
          assert(gui_focus_count == 3, 'cancelling an ordinary command should remain in Neovim')
          stage = 6
          vim.api.nvim_input(':' .. pending_command)
        elseif vim.g.argon_focus_replaced == 1 then
          assert(vim.g.argon_focus_pending == nil, 'a GUI command should abandon a pending command line')
          assert(gui_focus_count == 4, 'executing a GUI command should focus the GUI')
          vim.cmd('quitall!')
        end
      end)
    end, 10)
  end,
})

focus.editor('let g:argon_focus_stayed_in_editor = 1', { return_to_gui = false })
