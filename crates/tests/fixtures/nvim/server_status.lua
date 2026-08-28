local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

local echoes = {}
local original_echo = vim.api.nvim_echo
vim.api.nvim_echo = function(chunks, history, opts)
  echoes[#echoes + 1] = { chunks = chunks, history = history, opts = vim.deepcopy(opts) }
  return opts.id
end

local server_status = require('argon.server_status')

server_status.update(1, {
  token = 'first',
  value = { kind = 'begin', title = 'Another server task' },
})
assert(#echoes == 0, 'unrelated LSP progress should remain untouched')

server_status.update(1, {
  token = 'first',
  value = { kind = 'begin', title = 'Argon compilation' },
})
assert(echoes[#echoes].opts.status == 'running')
assert(echoes[#echoes].opts.id == 'argon.compilation')
assert(echoes[#echoes].chunks[1][1]:find('Compiling', 1, true))

server_status.update(2, {
  token = 2,
  value = { kind = 'begin', title = 'Argon compilation' },
})
assert(echoes[#echoes].chunks[1][1]:find('(2)', 1, true))

server_status.update(1, {
  token = 'first',
  value = { kind = 'end' },
})
assert(echoes[#echoes].opts.status == 'running')

server_status.reset_client_state(2)
assert(echoes[#echoes].opts.status == 'success')
assert(echoes[#echoes].chunks[1][1]:find('complete', 1, true))

vim.api.nvim_echo = original_echo
vim.cmd('quitall!')
