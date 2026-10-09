---@mod argon.agent Agent edits, applied to Neovim buffers by the analyzer.

local M = {}

local namespace = vim.api.nvim_create_namespace('argon_agent_edits')
local group = vim.api.nvim_create_augroup('argon_agent', { clear = true })

--- How long a changed range stays highlighted, in milliseconds.
local HIGHLIGHT_MS = 1500

local function define_highlight()
  vim.api.nvim_set_hl(0, 'ArgonAgentEdit', { default = true, link = 'IncSearch' })
end
define_highlight()
vim.api.nvim_create_autocmd('ColorScheme', { group = group, callback = define_highlight })

---The window that follows agent edits, or nil when follow mode is off.
---@type integer|nil
M.follow_win = nil

local function notify_follow_mode(client, enabled)
  if client.initialized and not client:is_stopped() then
    client:request('custom/followMode', { enabled = enabled }, function() end)
  end
end

local function broadcast_follow_mode()
  for _, client in ipairs(vim.lsp.get_clients({ name = 'argon' })) do
    notify_follow_mode(client, M.follow_win ~= nil)
  end
end

---Tells a newly initialized analyzer whether follow mode is on.
---@param client vim.lsp.Client
function M.on_init(client)
  notify_follow_mode(client, M.follow_win ~= nil)
end

---Turns follow mode on for the current window, or off.
---@param enabled boolean
function M.set_follow(enabled)
  M.follow_win = enabled and vim.api.nvim_get_current_win() or nil
  broadcast_follow_mode()
  vim.notify(
    enabled and 'argon: this window follows agent edits'
      or 'argon: no longer following agent edits',
    vim.log.levels.INFO
  )
end

vim.api.nvim_create_autocmd('WinClosed', {
  group = group,
  callback = function(args)
    if M.follow_win and tonumber(args.match) == M.follow_win then
      M.follow_win = nil
      broadcast_follow_mode()
    end
  end,
})

---Byte column of an LSP position.
---@param bufnr integer
---@param position lsp.Position
---@param encoding string
---@return integer
local function byte_col(bufnr, position, encoding)
  local line = vim.api.nvim_buf_get_lines(bufnr, position.line, position.line + 1, false)[1] or ''
  if encoding == 'utf-8' then
    return math.min(position.character, #line)
  end
  local ok, col = pcall(vim.str_byteindex, line, encoding, position.character, false)
  return ok and col or #line
end

local function display_name(bufnr)
  return vim.fn.fnamemodify(vim.api.nvim_buf_get_name(bufnr), ':~:.')
end

---Moves the follow window to `position`, unless the user is typing in it.
---@return boolean followed Whether a follow window exists.
local function follow(bufnr, position, encoding)
  local win = M.follow_win
  if not (win and vim.api.nvim_win_is_valid(win)) then
    return false
  end
  local typing = win == vim.api.nvim_get_current_win()
    and vim.api.nvim_get_mode().mode:match('^[iRc]') ~= nil
  if typing then
    return true
  end
  if vim.api.nvim_win_get_buf(win) ~= bufnr then
    vim.api.nvim_win_set_buf(win, bufnr)
  end
  local line = math.min(position.line + 1, vim.api.nvim_buf_line_count(bufnr))
  local col = byte_col(bufnr, { line = line - 1, character = position.character }, encoding)
  vim.api.nvim_win_set_cursor(win, { line, col })
  vim.api.nvim_win_call(win, function()
    vim.cmd('normal! zz')
  end)
  return true
end

---Highlights the text an agent edit wrote and brings it into view.
local function show(bufnr, params, encoding)
  for _, range in ipairs(params.highlights or {}) do
    local start_col = byte_col(bufnr, range.start, encoding)
    local end_col = byte_col(bufnr, range['end'], encoding)
    if range.start.line ~= range['end'].line or start_col ~= end_col then
      vim.hl.range(
        bufnr,
        namespace,
        'ArgonAgentEdit',
        { range.start.line, start_col },
        { range['end'].line, end_col },
        { timeout = HIGHLIGHT_MS }
      )
    end
  end
  local first = (params.highlights or {})[1]
  local followed = first and follow(bufnr, first.start, encoding)
  if not followed and vim.fn.bufwinid(bufnr) == -1 then
    vim.notify(
      ('argon: agent edited %s:%d (%s)'):format(
        display_name(bufnr),
        first and first.start.line + 1 or 1,
        params.label
      ),
      vim.log.levels.INFO
    )
  end
end

---Asks the user whether to apply an agent edit, previewing it as a diff.
---Tests replace this function.
---@param bufnr integer
---@param params table
---@return boolean accepted
function M.confirm(bufnr, params)
  local old = table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), '\n')
  if vim.bo[bufnr].eol then
    old = old .. '\n'
  end
  local diff = vim.text.diff(old, params.newText or '', { ctxlen = 3 }) or ''
  local lines = vim.split(diff, '\n', { trimempty = true })
  local width = 40
  for _, line in ipairs(lines) do
    width = math.max(width, vim.fn.strdisplaywidth(line))
  end
  local preview = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(preview, 0, -1, false, lines)
  vim.bo[preview].filetype = 'diff'
  local win = vim.api.nvim_open_win(preview, false, {
    relative = 'editor',
    row = 1,
    col = 2,
    width = math.max(1, math.min(vim.o.columns - 6, width)),
    height = math.max(1, math.min(vim.o.lines - 8, #lines)),
    style = 'minimal',
    border = 'rounded',
    title = ' Agent edit: ' .. display_name(bufnr) .. ' ',
    title_pos = 'center',
  })
  vim.cmd.redraw()
  local ok, choice = pcall(
    vim.fn.confirm,
    ('Apply agent edit "%s" to %s?'):format(params.label, display_name(bufnr)),
    '&Yes\n&No',
    1,
    'Question'
  )
  pcall(vim.api.nvim_win_close, win, true)
  pcall(vim.api.nvim_buf_delete, preview, { force = true })
  return ok and choice == 1
end

---Handles `custom/ensureBuffer`: loads a file into a listed buffer attached
---to the requesting analyzer, creating an empty buffer for a new file.
function M.ensure_buffer(params, ctx)
  local fname = vim.uri_to_fname(params.uri)
  if not params.create and vim.fn.filereadable(fname) ~= 1 then
    return { ok = false, message = fname .. ' does not exist' }
  end
  local bufnr = vim.fn.bufadd(fname)
  vim.bo[bufnr].buflisted = true
  if not vim.api.nvim_buf_is_loaded(bufnr) then
    vim.fn.bufload(bufnr)
  end
  if vim.bo[bufnr].filetype ~= 'argon' then
    vim.bo[bufnr].filetype = 'argon'
  end
  if not vim.lsp.buf_is_attached(bufnr, ctx.client_id) then
    vim.lsp.buf_attach_client(bufnr, ctx.client_id)
  end
  if not vim.lsp.buf_is_attached(bufnr, ctx.client_id) then
    return { ok = false, message = 'could not attach the Argon language server to ' .. fname }
  end
  return { ok = true }
end

---Handles `custom/applyAgentEdit`: applies the edits if the buffer is still
---at the version they were planned against, asking first when requested.
function M.apply_edit(params, ctx)
  local client = vim.lsp.get_client_by_id(ctx.client_id)
  if not client then
    return { status = 'failed', message = 'the Argon language client has stopped' }
  end
  local bufnr = vim.uri_to_bufnr(params.uri)
  if not vim.api.nvim_buf_is_loaded(bufnr) then
    return { status = 'failed', message = 'the buffer is not loaded' }
  end
  local version = vim.lsp.util.buf_versions[bufnr]
  if version ~= params.version then
    return { status = 'stale', version = version }
  end
  if params.confirm then
    if not M.confirm(bufnr, params) then
      return { status = 'rejected' }
    end
    version = vim.lsp.util.buf_versions[bufnr]
    if version ~= params.version then
      return { status = 'stale', version = version }
    end
  end
  local ok, err = pcall(vim.lsp.util.apply_text_edits, params.edits, bufnr, client.offset_encoding)
  if not ok then
    return { status = 'failed', message = tostring(err) }
  end
  show(bufnr, params, client.offset_encoding)
  return { status = 'applied', version = vim.lsp.util.buf_versions[bufnr] }
end

---Handles `custom/reloadBuffer`: rereads a file that changed on disk, unless
---its buffer has unsaved changes.
function M.reload_buffer(params, _)
  local fname = vim.uri_to_fname(params.uri)
  local bufnr
  for _, candidate in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_get_name(candidate) == fname then
      bufnr = candidate
      break
    end
  end
  if not bufnr or not vim.api.nvim_buf_is_loaded(bufnr) then
    return { status = 'closed' }
  end
  if vim.bo[bufnr].modified then
    return { status = 'modified' }
  end
  local before = vim.b[bufnr].changedtick
  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd('setlocal autoread')
    vim.cmd('silent! checktime ' .. bufnr)
    vim.cmd('setlocal autoread<')
  end)
  return { status = vim.b[bufnr].changedtick ~= before and 'reloaded' or 'unchanged' }
end

return M
