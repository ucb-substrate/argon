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
elseif vim.env.ARGON_TEST_MODE == 'navigation' then
  local client = vim.lsp.get_clients({ name = 'argon', bufnr = bufnr })[1]
  assert(
    client.offset_encoding == 'utf-8',
    'expected the server to negotiate utf-8, got ' .. tostring(client.offset_encoding)
  )
  assert(
    vim.bo[bufnr].tagfunc == 'v:lua.vim.lsp.tagfunc',
    'advertising definitionProvider should make <C-]> work'
  )
  assert(
    vim.fn.maparg('gd', 'n', false, true).buffer == 1,
    'the plugin should map gd in an attached buffer'
  )

  -- `width` on the last line refers to the `let` on the second.
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local use_line, use_column
  for index = #lines, 1, -1 do
    local column = lines[index]:find('width', 1, true)
    if column and not lines[index]:find('let width', 1, true) then
      use_line, use_column = index - 1, column - 1
      break
    end
  end
  assert(use_line, 'could not find a use of width')

  local params = {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    position = { line = use_line, character = use_column },
  }
  local function definition()
    local response = client:request_sync('textDocument/definition', params, 10000, bufnr)
    assert(response and not response.err, 'definition request failed: ' .. vim.inspect(response))
    if response.result == nil or vim.tbl_isempty(response.result) then
      return nil
    end
    return vim.islist(response.result) and response.result[1] or response.result
  end

  -- The index only exists once the debounced first compile has landed, and
  -- nothing notifies the client when that happens.
  local location
  wait_for('the workspace to be indexed', function()
    location = definition()
    return location ~= nil
  end)
  assert(
    vim.uri_to_fname(location.uri) == vim.fn.fnamemodify(vim.api.nvim_buf_get_name(bufnr), ':p'),
    'definition should be in the same file, got ' .. tostring(location.uri)
  )
  assert(
    lines[location.range.start.line + 1]:find('let width', 1, true),
    'definition should land on the let binding, got line '
      .. tostring(lines[location.range.start.line + 1])
  )

  params.context = { includeDeclaration = true }
  local references = client:request_sync('textDocument/references', params, 10000, bufnr)
  assert(references and not references.err, 'references request failed')
  assert(
    #references.result == 3,
    'expected the declaration and two uses, got ' .. tostring(#references.result)
  )

  -- Navigation must survive an edit that does not parse. Break the file after
  -- the position under test, so the position itself is still meaningful.
  vim.api.nvim_buf_set_lines(bufnr, -1, -1, false, { 'cell broken( {' })
  wait_for('the broken edit to reach the analyzer', function()
    return #vim.diagnostic.get(bufnr) > 0
  end)
  assert(
    definition() ~= nil,
    'navigation should keep answering from the last index that compiled'
  )
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
