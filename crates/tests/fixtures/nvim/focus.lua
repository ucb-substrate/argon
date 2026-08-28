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
local exit_keys = { '<CR>', '<CR>', '<Esc>', '<C-C>', '<Esc>' }

local function checked(callback)
  local ok, err = pcall(callback)
  if not ok then
    vim.api.nvim_err_writeln(tostring(err))
    vim.cmd('cquit')
  end
end

vim.api.nvim_create_autocmd('CmdlineEnter', {
  callback = function()
    local exit_key = assert(exit_keys[stage])
    vim.schedule(function()
      vim.api.nvim_input(exit_key)
    end)
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
        else
          assert(vim.g.argon_ordinary_command == nil)
          assert(gui_focus_count == 3, 'cancelling an ordinary command should remain in Neovim')
          vim.cmd('quitall!')
        end
      end)
    end, 10)
  end,
})

focus.editor('let g:argon_focus_stayed_in_editor = 1', { return_to_gui = false })
