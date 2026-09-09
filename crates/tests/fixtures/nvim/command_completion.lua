local project_root = assert(vim.env.ARGON_REPOSITORY_ROOT)
vim.opt.runtimepath:append(project_root)

--- Canned response for the next synchronous request, or `nil` to fail it.
local response = nil
--- Parameters the completion module last sent.
local sent = nil
--- Parameters of every asynchronous request, which `warm` uses.
local async = {}

package.loaded['argon.client'] = {
  request_sync_first = function(method, params, timeout_ms)
    assert(method == 'custom/commandCompletion', method)
    assert(type(timeout_ms) == 'number' and timeout_ms > 0, 'a timeout is required')
    sent = params
    return response
  end,
  any_buf_request = function(method, params, handler)
    assert(method == 'custom/commandCompletion', method)
    table.insert(async, params)
    handler(nil, { prefixLen = 0, items = { { label = 'cached_cell', insertText = 'cached_cell(' } } })
  end,
  print_error = function() end,
}

local completion = require('argon.commands.completion')
local open_cell = completion.cell_expression('openCell')
local inst = completion.cell_expression('inst')

local function items(...)
  local list = {}
  for _, label in ipairs({ ... }) do
    table.insert(list, { label = label, insertText = label .. '(' })
  end
  return list
end

local function keywords(...)
  local list = {}
  for _, label in ipairs({ ... }) do
    table.insert(list, { label = label, insertText = label .. '=' })
  end
  return list
end

---Completes `cmdline` with the cursor at its end, deriving `arg_lead` the way
---Neovim does: the text after the last space.
local function complete(callback, cmdline)
  local arg_lead = cmdline:match('%S*$')
  return callback(nil, arg_lead, cmdline, #cmdline)
end

-- A candidate replaces the whole of `arg_lead`, so anything before the
-- identifier being completed has to come back with it.
response = { prefixLen = 2, items = items('child', 'chip', 'top') }
assert(
  vim.deep_equal(complete(open_cell, 'Argon openCell ch'), { 'child(', 'chip(' }),
  vim.inspect(complete(open_cell, 'Argon openCell ch'))
)
assert(sent.command == 'openCell', vim.inspect(sent))
assert(sent.text == 'ch' and sent.cursor == 2, vim.inspect(sent))

response = { prefixLen = 2, items = keywords('layer') }
assert(
  vim.deep_equal(complete(open_cell, 'Argon openCell child(la'), { 'child(layer=' }),
  vim.inspect(complete(open_cell, 'Argon openCell child(la'))
)
assert(sent.text == 'child(la' and sent.cursor == 8, vim.inspect(sent))

-- A space inside the invocation ends `arg_lead`, so only the keyword is
-- replaced, while the request still carries the whole argument.
response = { prefixLen = 2, items = keywords('layer') }
assert(
  vim.deep_equal(complete(open_cell, 'Argon openCell child(1., la'), { 'layer=' }),
  vim.inspect(complete(open_cell, 'Argon openCell child(1., la'))
)
assert(sent.text == 'child(1., la' and sent.cursor == 12, vim.inspect(sent))

-- Nothing precedes an identifier at the start of the argument.
response = { prefixLen = 0, items = items('child') }
assert(vim.deep_equal(complete(open_cell, 'Argon openCell '), { 'child(' }))
assert(sent.text == '' and sent.cursor == 0, vim.inspect(sent))

-- The argument is read up to the cursor, not to the end of the line.
response = { prefixLen = 2, items = keywords('layer') }
local trailing = 'Argon openCell child(la'
assert(vim.deep_equal(open_cell(nil, 'child(la', trailing .. ', 1.)', #trailing), { 'child(layer=' }))
assert(sent.text == 'child(la', vim.inspect(sent))

-- A range or a bang may precede the command name.
response = { prefixLen = 0, items = items('child') }
assert(vim.deep_equal(complete(open_cell, "'<,'>Argon openCell "), { 'child(' }))
assert(vim.deep_equal(complete(open_cell, 'Argon! openCell '), { 'child(' }))

-- Each subcommand completes its own argument and nothing else.
response = { prefixLen = 0, items = items('child') }
assert(vim.deep_equal(complete(inst, 'Argon inst '), { 'child(' }))
assert(sent.command == 'inst', vim.inspect(sent))
assert(vim.deep_equal(complete(open_cell, 'Argon inst '), {}))
assert(vim.deep_equal(complete(open_cell, 'Argon newCell foo'), {}))
assert(vim.deep_equal(complete(open_cell, 'Argon openCell'), {}))

-- An analyzer that does not answer falls back to the last response for the
-- same position, filtered by whatever has been typed since.
response = { prefixLen = 0, items = items('child', 'chip', 'top') }
complete(open_cell, 'Argon openCell ')
response = nil
assert(
  vim.deep_equal(complete(open_cell, 'Argon openCell ch'), { 'child(', 'chip(' }),
  vim.inspect(complete(open_cell, 'Argon openCell ch'))
)
-- A different position is not what the cache holds, so nothing is offered
-- rather than candidates from elsewhere in the expression.
assert(vim.deep_equal(complete(open_cell, 'Argon openCell child(la'), {}))

-- An empty response leaves the command line untouched rather than offering
-- the cached candidates from an earlier position.
response = { prefixLen = 0, items = {} }
assert(vim.deep_equal(complete(open_cell, 'Argon openCell child(1., 2.)'), {}))

-- A numeric prefix matches no name.
response = { prefixLen = 4, items = items('child') }
assert(vim.deep_equal(complete(open_cell, 'Argon openCell child(2000'), {}))

-- Warming caches the callee position for both subcommands without blocking.
completion.warm()
assert(#async == 2, vim.inspect(async))
assert(async[1].command == 'openCell' and async[1].text == '', vim.inspect(async))
assert(async[2].command == 'inst' and async[2].text == '', vim.inspect(async))
response = nil
assert(vim.deep_equal(complete(open_cell, 'Argon openCell cached'), { 'cached_cell(' }))

-- The dispatcher routes to the right subcommand and completes subcommand
-- names by prefix.
local commands = require('argon.commands')
commands.create_argon_command()
response = { prefixLen = 2, items = items('child') }
assert(
  vim.deep_equal(vim.fn.getcompletion('Argon openCell ch', 'cmdline'), { 'child(' }),
  vim.inspect(vim.fn.getcompletion('Argon openCell ch', 'cmdline'))
)
local names = vim.fn.getcompletion('Argon open', 'cmdline')
assert(vim.deep_equal(names, { 'openCell' }), vim.inspect(names))
-- `Cell` appears inside three subcommand names but starts none of them.
assert(vim.deep_equal(vim.fn.getcompletion('Argon Cell', 'cmdline'), {}))

vim.cmd('quitall!')
