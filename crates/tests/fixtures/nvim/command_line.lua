local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

local focus = require('argon.focus')
local commands = vim.json.decode(assert(vim.env.ARGON_EDITOR_COMMANDS))
local expected = {
  'messages<CR>',
  'Argon inst ',
  'Argon openCell pad<lt>1>(',
  'Argon diagnostics<CR>',
}
assert(#commands == #expected, vim.inspect(commands))
for index, command in ipairs(commands) do
  local line = focus.command_line(command)
  assert(line == expected[index], vim.inspect({ command = command, line = line }))
end
assert(focus.command_line(nil) == nil)
assert(focus.command_line(vim.NIL) == nil)
