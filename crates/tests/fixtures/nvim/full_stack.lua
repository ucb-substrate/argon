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

local compilation_tokens = {}
local compilation_begins = 0
local compilation_ends = 0
vim.api.nvim_create_autocmd('LspProgress', {
  callback = function(args)
    local client = vim.lsp.get_client_by_id(args.data.client_id)
    local params = args.data.params
    local value = params.value
    if not client or client.name ~= 'argon' or type(value) ~= 'table' then
      return
    end
    local token = type(params.token) .. ':' .. tostring(params.token)
    if value.kind == 'begin' and value.title == 'Argon compilation' then
      compilation_tokens[token] = true
      compilation_begins = compilation_begins + 1
    elseif value.kind == 'end' and compilation_tokens[token] then
      compilation_tokens[token] = nil
      compilation_ends = compilation_ends + 1
    end
  end,
})

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
  for _, capability in ipairs({
    'completionProvider',
    'hoverProvider',
    'signatureHelpProvider',
    'documentSymbolProvider',
    'documentHighlightProvider',
  }) do
    assert(client.server_capabilities[capability], 'missing LSP capability ' .. capability)
  end

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

  local completion = client:request_sync('textDocument/completion', params, 10000, bufnr)
  assert(completion and not completion.err, 'completion request failed')
  local completion_items = completion.result.items or completion.result
  local completion_labels = {}
  for _, item in ipairs(completion_items) do
    completion_labels[item.label] = true
  end
  assert(completion_labels.width, 'completion should contain the visible local width')
  assert(completion_labels.rect, 'completion should contain builtin functions')

  local hover = client:request_sync('textDocument/hover', params, 10000, bufnr)
  assert(hover and not hover.err and hover.result, 'hover request failed')
  assert(
    hover.result.contents.value:find('let width: Float', 1, true),
    'hover should show the inferred local type: ' .. vim.inspect(hover.result)
  )

  local highlights = client:request_sync('textDocument/documentHighlight', params, 10000, bufnr)
  assert(highlights and not highlights.err, 'document-highlight request failed')
  assert(
    #highlights.result == 3,
    'expected the width declaration and two uses to be highlighted, got '
      .. tostring(#highlights.result)
  )

  local rect_column = assert(lines[3]:find('rect(', 1, true)) - 1 + #'rect('
  local signature = client:request_sync('textDocument/signatureHelp', {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    position = { line = 2, character = rect_column },
  }, 10000, bufnr)
  assert(signature and not signature.err and signature.result, 'signature-help request failed')
  assert(
    signature.result.signatures[1].label:find('fn rect(', 1, true) == 1,
    'signature help should describe rect: ' .. vim.inspect(signature.result)
  )

  local symbols = client:request_sync('textDocument/documentSymbol', {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
  }, 10000, bufnr)
  assert(symbols and not symbols.err, 'document-symbol request failed')
  local symbol_names = {}
  for _, symbol in ipairs(symbols.result) do
    symbol_names[symbol.name] = true
  end
  assert(symbol_names.top and symbol_names.width, 'document outline is incomplete')

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

  -- A broken edit *before* the position under test shifts every offset after
  -- it. The retained index still describes the file as it was, so the answer
  -- has to be translated rather than read off the stale offsets directly.
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
  wait_for('the restored file to compile', function()
    return #vim.diagnostic.get(bufnr) == 0
  end)
  vim.api.nvim_buf_set_lines(bufnr, 0, 0, false, { 'cell broken( {' })
  wait_for('the leading broken edit to reach the analyzer', function()
    return #vim.diagnostic.get(bufnr) > 0
  end)
  params.position = { line = use_line + 1, character = use_column }
  local shifted = definition()
  assert(shifted, 'navigation should survive a broken edit above the cursor')
  assert(
    shifted.range.start.line == location.range.start.line + 1,
    'the definition should follow the inserted line, expected '
      .. tostring(location.range.start.line + 1)
      .. ' got '
      .. tostring(shifted.range.start.line)
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

wait_for('Argon compilation progress to finish', function()
  return compilation_begins > 0 and compilation_ends == compilation_begins
end)

assert(#vim.lsp.get_clients({ name = 'argon', bufnr = bufnr }) == 1)
vim.cmd('quitall!')
