local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

vim.g.argon_embedded_gui = true
vim.g.argon_analyzer_relay = '/tmp/argone-test/analyzer.sock'
vim.g.argon = { cmd = { 'argon-analyzer' } }

local started = {}
vim.lsp.start = function(config)
  table.insert(started, config)
  return nil
end

local argon = require('argon')
for _, mode in ipairs({ 'n', 'i', 'v', 'c', 't' }) do
  assert(vim.fn.maparg('<C-\\>', mode) == '', 'the embedded GUI leaves <C-\\> to Neovim in mode ' .. mode)
end

local directory = vim.fn.tempname()
vim.fn.mkdir(directory, 'p')
vim.fn.writefile({ 'cell top() {}' }, directory .. '/lib.ar')
vim.cmd.edit(directory .. '/lib.ar')
argon.start(vim.api.nvim_get_current_buf())
local cmd = assert(started[#started], 'the analyzer should start').cmd
assert(
  vim.deep_equal(cmd, {
    'argon-analyzer',
    '--relay-socket',
    '/tmp/argone-test/analyzer.sock',
    '--embedded-gui',
  }),
  vim.inspect(cmd)
)
