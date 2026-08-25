local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

local diagnostics = require('argon.diagnostics')

local first = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(first, project_root .. '/first.ar')
vim.api.nvim_buf_set_lines(first, 0, -1, false, { 'cell first() {', '  missing', '}' })

local second = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(second, project_root .. '/nested/second.ar')
vim.api.nvim_buf_set_lines(second, 0, -1, false, { 'cell second() {', '  warning', '}' })

diagnostics.open({
  {
    bufnr = first,
    lnum = 1,
    col = 2,
    end_lnum = 1,
    end_col = 9,
    severity = vim.diagnostic.severity.ERROR,
    message = 'unknown identifier',
  },
  {
    bufnr = second,
    lnum = 1,
    col = 2,
    end_lnum = 1,
    end_col = 9,
    severity = vim.diagnostic.severity.WARN,
    message = 'unused value',
  },
})

local panel = vim.api.nvim_get_current_buf()
local lines = vim.api.nvim_buf_get_lines(panel, 0, -1, false)
local output = table.concat(lines, '\n')
assert(output:find('1 error, 1 warning in 2 files', 1, true))
assert(output:find('error: unknown identifier', 1, true))
assert(output:find('warning: unused value', 1, true))
assert(output:find('first.ar:2:3', 1, true))
assert(output:find('nested/second.ar:2:3', 1, true))
assert(output:find('2 |   missing', 1, true))
assert(output:find('2 |   warning', 1, true))

local quickfix = vim.fn.getqflist({ title = 1, items = 1 })
assert(quickfix.title == 'Argon diagnostics')
assert(#quickfix.items == 2)
assert(quickfix.items[1].bufnr == first)
assert(quickfix.items[2].bufnr == second)

for _, lhs in ipairs({ '<CR>', ']d', '[d', 'r', 'q' }) do
  assert(vim.fn.maparg(lhs, 'n', false, true).buffer == 1, 'missing panel mapping: ' .. lhs)
end

local warning_line
for index, line in ipairs(lines) do
  if line == 'warning: unused value' then
    warning_line = index
    break
  end
end
assert(warning_line)
vim.api.nvim_win_set_cursor(0, { warning_line, 0 })
vim.fn.maparg('<CR>', 'n', false, true).callback()
assert(vim.api.nvim_get_current_buf() == second)
assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 2, 2 }))

vim.cmd('quitall!')
