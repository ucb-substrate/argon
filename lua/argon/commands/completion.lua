---@mod argon.commands.completion Completion for `:Argon` command arguments.
---
---The command line has no document for the analyzer to complete against, so
---the argument typed so far is sent as text and resolved against the
---navigation index. Neovim's command-line completion is synchronous, so the
---request blocks briefly and falls back to the last response for the same
---position when the analyzer does not answer in time.

local M = {}

local client = require('argon.client')

--- How long to keep the command line waiting for the analyzer.
local TIMEOUT_MS = 150

--- Subcommands whose argument is a cell invocation.
local CELL_EXPRESSION_COMMANDS = { 'openCell', 'inst' }

--- Last response per subcommand, keyed by the argument text preceding the
--- identifier it completed. A later keystroke in the same position filters
--- that response instead of waiting for a new one.
---@type table<string, { key: string, items: table[] }>
local cache = {}

---Length of the identifier at the end of `text`, which is the part of the
---argument a candidate replaces.
---@param text string
---@return integer
local function identifier_prefix_len(text)
  return #(text:match('[%w_]*$') or '')
end

---The argument text from the start of the argument up to the cursor.
---
---Completion needs the whole argument, not just `arg_lead`: Neovim delimits
---the lead by whitespace, so only `la` leads in `openCell child(1., la`.
---@param cmdline string
---@param cursor_pos integer
---@param subcommand string
---@return string|nil
local function argument_before_cursor(cmdline, cursor_pos, subcommand)
  local _, argument_start = cmdline:find("^%s*['<,'>]*Argon!?%s+" .. subcommand .. '%s')
  if not argument_start then
    return nil
  end
  local cursor = math.min(cursor_pos, #cmdline)
  if cursor < argument_start then
    return nil
  end
  return cmdline:sub(argument_start + 1, cursor)
end

---@param subcommand string
---@param text string The argument up to the cursor
---@return table[]|nil items, integer prefix_len
local function request(subcommand, text)
  local result = client.request_sync_first('custom/commandCompletion', {
    command = subcommand,
    text = text,
    cursor = #text,
  }, TIMEOUT_MS)
  if not result then
    return nil, identifier_prefix_len(text)
  end
  local prefix_len = result.prefixLen or 0
  cache[subcommand] = { key = text:sub(1, #text - prefix_len), items = result.items or {} }
  return result.items or {}, prefix_len
end

---Candidates for a cell invocation argument, as command-line replacements.
---
---A candidate replaces `arg_lead` in full, so whatever precedes the completed
---identifier is prepended: completing `width` in `child(1., wi` must offer
---`width`, and in `child(wi` must offer `child(width`.
---@param subcommand string
---@param text string The argument up to the cursor
---@param arg_lead string
---@return string[]
local function candidates(subcommand, text, arg_lead)
  local items, prefix_len = request(subcommand, text)
  -- Clamp before use: a prefix longer than what precedes the cursor would
  -- index from the end of the lead and insert the wrong text.
  prefix_len = math.min(prefix_len, #arg_lead, #text)
  local key = text:sub(1, #text - prefix_len)
  if not items then
    local cached = cache[subcommand]
    if not cached or cached.key ~= key then
      return {}
    end
    items = cached.items
  end

  local prefix = text:sub(#text - prefix_len + 1)
  local head = arg_lead:sub(1, #arg_lead - prefix_len)
  local matches = {}
  for _, item in ipairs(items) do
    if vim.startswith(item.label, prefix) then
      table.insert(matches, head .. (item.insertText or item.label))
    end
  end
  return matches
end

---Completion callback for a subcommand whose argument is a cell invocation.
---@param subcommand string
---@return fun(subcmd_arg_lead: string, arg_lead: string, cmdline: string, cursor_pos: integer): string[]
function M.cell_expression(subcommand)
  return function(_, arg_lead, cmdline, cursor_pos)
    local text = argument_before_cursor(cmdline, cursor_pos, subcommand)
    if not text then
      return {}
    end
    return candidates(subcommand, text, arg_lead or '')
  end
end

---Fill the cache for an empty argument without blocking, so the first
---completion after the GUI opens a command line is answered locally.
function M.warm()
  for _, subcommand in ipairs(CELL_EXPRESSION_COMMANDS) do
    client.any_buf_request('custom/commandCompletion', {
      command = subcommand,
      text = '',
      cursor = 0,
    }, function(err, result)
      if not err and result then
        cache[subcommand] = { key = '', items = result.items or {} }
      end
    end)
  end
end

return M
