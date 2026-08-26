local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

local save = require('argon.save')
local client_id = 42
local directory = vim.fn.tempname()
vim.fn.mkdir(directory, 'p')

local function buffer(name, filetype, contents)
  local path = directory .. '/' .. name
  vim.fn.writefile({ 'on disk' }, path)
  local bufnr = vim.fn.bufadd(path)
  vim.fn.bufload(bufnr)
  vim.bo[bufnr].filetype = filetype
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, { contents })
  return bufnr, path
end

local first, first_path = buffer('first.ar', 'argon', 'first unsaved')
local second, second_path = buffer('second.ar', 'argon', 'second unsaved')
local unrelated, unrelated_path = buffer('unrelated.ar', 'argon', 'unrelated unsaved')
local non_argon, non_argon_path = buffer('notes.txt', 'text', 'notes unsaved')
local buffers = { first, second, unrelated, non_argon }

local old_get_buffers = vim.lsp.get_buffers_by_client_id
local old_is_attached = vim.lsp.buf_is_attached
local old_get_client = vim.lsp.get_client_by_id
local notification
vim.lsp.get_buffers_by_client_id = function(id)
  assert(id == client_id)
  return buffers
end
vim.lsp.buf_is_attached = function(bufnr, id)
  return id == client_id and bufnr ~= unrelated
end
vim.lsp.get_client_by_id = function(id)
  assert(id == client_id)
  return {
    name = 'argon',
    notify = function(_, method, params)
      notification = { method = method, params = params }
    end,
  }
end

assert(save.workspace_modified(client_id))
save.notify_workspace_modified(client_id)
assert(notification.method == 'custom/workspaceModified')
assert(notification.params.modified)

save.save_modified_buffers(client_id)

assert(vim.fn.readfile(first_path)[1] == 'first unsaved')
assert(vim.fn.readfile(second_path)[1] == 'second unsaved')
assert(vim.fn.readfile(unrelated_path)[1] == 'on disk')
assert(vim.fn.readfile(non_argon_path)[1] == 'on disk')
assert(not vim.bo[first].modified)
assert(not vim.bo[second].modified)
assert(vim.bo[unrelated].modified)
assert(vim.bo[non_argon].modified)
assert(not save.workspace_modified(client_id))
save.notify_workspace_modified(client_id)
assert(not notification.params.modified)

vim.lsp.get_buffers_by_client_id = old_get_buffers
vim.lsp.buf_is_attached = old_is_attached
vim.lsp.get_client_by_id = old_get_client
vim.fn.delete(directory, 'rf')
vim.cmd('quitall!')
