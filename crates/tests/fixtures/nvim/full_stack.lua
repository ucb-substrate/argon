local deadline = vim.uv.hrtime() + 20 * 1000000000

local function wait_for(description, predicate)
  if predicate() then
    return
  end
  local remaining_ms = math.floor((deadline - vim.uv.hrtime()) / 1000000)
  assert(
    remaining_ms > 0 and vim.wait(remaining_ms, predicate, 25),
    'timed out waiting for ' .. description
  )
end

local bufnr = vim.api.nvim_get_current_buf()
wait_for('Argon language server', function()
  return #vim.lsp.get_clients({ name = 'argon', bufnr = bufnr }) == 1
    and vim.fn.exists(':Argon') == 2
end)

if vim.env.ARGON_TEST_MODE ~= 'rpc_errors' then
  vim.cmd('Argon openCell top()')
end

if vim.env.ARGON_TEST_MODE == 'roundtrip' then
  wait_for('GUI source edit', function()
    return table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), '\n')
      :find('let gui_rect = rect(', 1, true) ~= nil
  end)
  wait_for('GUI edit to recompile', function()
    return vim.uv.fs_stat(vim.env.ARGON_TEST_GUI_EDIT_ACK) ~= nil
  end)
  assert(vim.bo[bufnr].modified, 'GUI source edit should leave the buffer modified')

  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local closing_line
  for index = #lines, 1, -1 do
    if lines[index] == '}' then
      closing_line = index
      break
    end
  end
  assert(closing_line, 'could not find cell closing brace')
  vim.api.nvim_buf_set_lines(bufnr, closing_line - 1, closing_line - 1, false, {
    '    let editor_rect = rect("met1", x0i = 20., y0i = 20., x1i = 30., y1i = 30.)!;',
  })
  vim.cmd('write')
elseif vim.env.ARGON_TEST_MODE == 'diagnostics' then
  wait_for('analyzer diagnostics', function()
    return #vim.diagnostic.get(bufnr) > 0
  end)
  wait_for('diagnostics to reach GUI', function()
    return vim.uv.fs_stat(vim.env.ARGON_TEST_DIAGNOSTIC_ACK) ~= nil
  end)
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, { 'cell top() {', '}' })
  vim.cmd('write')
  wait_for('diagnostics to clear', function()
    return #vim.diagnostic.get(bufnr) == 0
  end)
elseif vim.env.ARGON_TEST_MODE == 'rpc_errors' then
  -- The Rust test drives the analyzer RPC directly and acknowledges the
  -- mirrored GUI error after observing it.
else
  error('unknown full-stack mode: ' .. tostring(vim.env.ARGON_TEST_MODE))
end

wait_for('headless GUI acknowledgement', function()
  return vim.uv.fs_stat(vim.env.ARGON_TEST_ACK) ~= nil
end)

assert(#vim.lsp.get_clients({ name = 'argon', bufnr = bufnr }) == 1)
vim.cmd('quitall!')
