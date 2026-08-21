local M = {}

local panel = {
  bufnr = nil,
  source_winid = nil,
  entries = {},
  line_targets = {},
  quickfix_id = nil,
}
local highlight_namespace = vim.api.nvim_create_namespace('argon-diagnostics-panel')

local severity_labels = {
  [vim.diagnostic.severity.ERROR] = 'error',
  [vim.diagnostic.severity.WARN] = 'warning',
  [vim.diagnostic.severity.INFO] = 'info',
  [vim.diagnostic.severity.HINT] = 'hint',
}

local severity_highlights = {
  [vim.diagnostic.severity.ERROR] = 'DiagnosticError',
  [vim.diagnostic.severity.WARN] = 'DiagnosticWarn',
  [vim.diagnostic.severity.INFO] = 'DiagnosticInfo',
  [vim.diagnostic.severity.HINT] = 'DiagnosticHint',
}

local severity_types = {
  [vim.diagnostic.severity.ERROR] = 'E',
  [vim.diagnostic.severity.WARN] = 'W',
  [vim.diagnostic.severity.INFO] = 'I',
  [vim.diagnostic.severity.HINT] = 'N',
}

local function argon_namespaces()
  local namespaces = {}
  for _, lsp_client in ipairs(require('argon.client').get_active_argon_lsp_clients(nil)) do
    table.insert(namespaces, vim.lsp.diagnostic.get_namespace(lsp_client.id))
  end
  return namespaces
end

---@return vim.Diagnostic[]
function M.get()
  local diagnostics = {}
  for _, namespace in ipairs(argon_namespaces()) do
    vim.list_extend(diagnostics, vim.diagnostic.get(nil, { namespace = namespace }))
  end
  table.sort(diagnostics, function(a, b)
    local a_name = vim.api.nvim_buf_get_name(a.bufnr)
    local b_name = vim.api.nvim_buf_get_name(b.bufnr)
    if a_name ~= b_name then
      return a_name < b_name
    end
    if a.lnum ~= b.lnum then
      return a.lnum < b.lnum
    end
    return a.col < b.col
  end)
  return diagnostics
end

local function quickfix_items(diagnostics)
  return vim.tbl_map(function(diagnostic)
    return {
      bufnr = diagnostic.bufnr,
      lnum = diagnostic.lnum + 1,
      col = diagnostic.col + 1,
      end_lnum = (diagnostic.end_lnum or diagnostic.lnum) + 1,
      end_col = (diagnostic.end_col or diagnostic.col) + 1,
      text = diagnostic.message,
      type = severity_types[diagnostic.severity] or 'E',
      valid = 1,
    }
  end, diagnostics)
end

---@param diagnostics? vim.Diagnostic[]
function M.set_quickfix(diagnostics)
  diagnostics = diagnostics or M.get()
  local action = ' '
  local properties = {
    title = 'Argon diagnostics',
    items = quickfix_items(diagnostics),
  }
  if panel.quickfix_id then
    local quickfix = vim.fn.getqflist({ id = panel.quickfix_id })
    if quickfix.id == panel.quickfix_id then
      action = 'r'
      properties.id = panel.quickfix_id
    else
      panel.quickfix_id = nil
    end
  end
  vim.fn.setqflist({}, action, properties)
  if not panel.quickfix_id then
    panel.quickfix_id = vim.fn.getqflist({ id = 0 }).id
  end
end

local function source_line(diagnostic)
  if not vim.api.nvim_buf_is_loaded(diagnostic.bufnr) then
    vim.fn.bufload(diagnostic.bufnr)
  end
  return vim.api.nvim_buf_get_lines(diagnostic.bufnr, diagnostic.lnum, diagnostic.lnum + 1, false)[1]
end

local function display_path(bufnr)
  local path = vim.api.nvim_buf_get_name(bufnr)
  if path == '' then
    return '[No Name]'
  end
  return vim.fn.fnamemodify(path, ':~:.')
end

local function underline(diagnostic, line)
  local prefix = line:sub(1, diagnostic.col)
  local start = vim.fn.strdisplaywidth(prefix)
  local finish = diagnostic.end_col or diagnostic.col + 1
  local width = 1
  if (diagnostic.end_lnum or diagnostic.lnum) == diagnostic.lnum then
    width = math.max(1, vim.fn.strdisplaywidth(line:sub(diagnostic.col + 1, finish)))
  end
  return string.rep(' ', start) .. string.rep('^', width)
end

local function diagnostic_counts(diagnostics)
  local counts = {}
  local files = {}
  for _, diagnostic in ipairs(diagnostics) do
    local severity = diagnostic.severity or vim.diagnostic.severity.ERROR
    counts[severity] = (counts[severity] or 0) + 1
    files[diagnostic.bufnr] = true
  end
  return counts, vim.tbl_count(files)
end

local function add_target(line_targets, entries, first, last, diagnostic)
  local target = {
    bufnr = diagnostic.bufnr,
    lnum = diagnostic.lnum,
    col = diagnostic.col,
    panel_line = first,
  }
  table.insert(entries, target)
  for line = first, last do
    line_targets[line] = target
  end
end

local function render(diagnostics)
  local counts, files = diagnostic_counts(diagnostics)
  local summary_parts = {}
  for _, severity in ipairs({
    vim.diagnostic.severity.ERROR,
    vim.diagnostic.severity.WARN,
    vim.diagnostic.severity.INFO,
    vim.diagnostic.severity.HINT,
  }) do
    local count = counts[severity] or 0
    if count > 0 then
      local label = severity_labels[severity]
      table.insert(summary_parts, string.format('%d %s%s', count, label, count == 1 and '' or 's'))
    end
  end
  if #summary_parts == 0 then
    table.insert(summary_parts, '0 diagnostics')
  end
  local summary = 'Argon diagnostics — ' .. table.concat(summary_parts, ', ')
  summary = summary .. string.format(' in %d file%s', files, files == 1 and '' or 's')

  local lines = { summary, '' }
  local entries = {}
  local line_targets = {}
  local highlights = {
    { line = 0, start_col = 0, end_col = -1, group = 'Title' },
  }

  if #diagnostics == 0 then
    table.insert(lines, 'No diagnostics.')
    table.insert(highlights, { line = 2, start_col = 0, end_col = -1, group = 'Comment' })
    return lines, entries, line_targets, highlights
  end

  for _, diagnostic in ipairs(diagnostics) do
    local first = #lines + 1
    local severity = diagnostic.severity or vim.diagnostic.severity.ERROR
    local label = severity_labels[severity] or 'error'
    local messages = vim.split(diagnostic.message, '\n', { plain = true })
    table.insert(lines, string.format('%s: %s', label, messages[1]))
    table.insert(highlights, {
      line = #lines - 1,
      start_col = 0,
      end_col = #label,
      group = severity_highlights[severity] or 'DiagnosticError',
    })
    for index = 2, #messages do
      table.insert(lines, string.rep(' ', #label + 2) .. messages[index])
    end

    local line_number = diagnostic.lnum + 1
    local column = diagnostic.col + 1
    table.insert(lines, string.format('  --> %s:%d:%d', display_path(diagnostic.bufnr), line_number, column))
    table.insert(highlights, {
      line = #lines - 1,
      start_col = 2,
      end_col = -1,
      group = 'Directory',
    })

    local line = source_line(diagnostic)
    if line then
      local gutter = math.max(1, #tostring(line_number))
      table.insert(lines, string.format('%' .. gutter .. 's |', ''))
      table.insert(lines, string.format('%' .. gutter .. 'd | %s', line_number, line))
      table.insert(lines, string.format('%' .. gutter .. 's | %s', '', underline(diagnostic, line)))
      local caret_start = lines[#lines]:find('%^') - 1
      table.insert(highlights, {
        line = #lines - 1,
        start_col = caret_start,
        end_col = -1,
        group = severity_highlights[severity] or 'DiagnosticError',
      })
    end
    table.insert(lines, '')
    add_target(line_targets, entries, first, #lines, diagnostic)
  end

  return lines, entries, line_targets, highlights
end

local function target_at_cursor()
  local line = vim.api.nvim_win_get_cursor(0)[1]
  return panel.line_targets[line]
end

local function jump_to_target(target)
  if not target then
    return
  end
  local winid = panel.source_winid
  if not winid or not vim.api.nvim_win_is_valid(winid) then
    winid = vim.fn.win_getid(vim.fn.winnr('#'))
  end
  if winid == 0 or not vim.api.nvim_win_is_valid(winid) then
    vim.cmd('aboveleft new')
    winid = vim.api.nvim_get_current_win()
  else
    vim.api.nvim_set_current_win(winid)
  end
  vim.api.nvim_win_set_buf(winid, target.bufnr)
  vim.api.nvim_win_set_cursor(winid, { target.lnum + 1, target.col })
end

local function move_to_entry(direction)
  if #panel.entries == 0 then
    return
  end
  local cursor_line = vim.api.nvim_win_get_cursor(0)[1]
  local selected
  if direction > 0 then
    for _, entry in ipairs(panel.entries) do
      if entry.panel_line > cursor_line then
        selected = entry
        break
      end
    end
    selected = selected or panel.entries[1]
  else
    for index = #panel.entries, 1, -1 do
      if panel.entries[index].panel_line < cursor_line then
        selected = panel.entries[index]
        break
      end
    end
    selected = selected or panel.entries[#panel.entries]
  end
  vim.api.nvim_win_set_cursor(0, { selected.panel_line, 0 })
end

local function configure_panel(bufnr)
  vim.bo[bufnr].buftype = 'nofile'
  vim.bo[bufnr].bufhidden = 'wipe'
  vim.bo[bufnr].swapfile = false
  vim.bo[bufnr].modifiable = false
  vim.bo[bufnr].filetype = 'argon-diagnostics'
  vim.bo[bufnr].undolevels = -1

  local map_opts = { buffer = bufnr, silent = true }
  vim.keymap.set('n', '<CR>', function()
    jump_to_target(target_at_cursor())
  end, vim.tbl_extend('force', map_opts, { desc = 'Open diagnostic' }))
  vim.keymap.set('n', ']d', function()
    move_to_entry(1)
  end, vim.tbl_extend('force', map_opts, { desc = 'Next Argon diagnostic' }))
  vim.keymap.set('n', '[d', function()
    move_to_entry(-1)
  end, vim.tbl_extend('force', map_opts, { desc = 'Previous Argon diagnostic' }))
  vim.keymap.set('n', 'r', function()
    M.refresh()
  end, vim.tbl_extend('force', map_opts, { desc = 'Refresh Argon diagnostics' }))
  vim.keymap.set('n', 'q', '<Cmd>close<CR>', vim.tbl_extend('force', map_opts, {
    desc = 'Close Argon diagnostics',
  }))
end

local function panel_window()
  if panel.bufnr and vim.api.nvim_buf_is_valid(panel.bufnr) then
    local winid = vim.fn.bufwinid(panel.bufnr)
    if winid ~= -1 then
      return winid
    end
  end
end

local function set_panel_contents(diagnostics)
  local lines, entries, line_targets, highlights = render(diagnostics)
  panel.entries = entries
  panel.line_targets = line_targets
  vim.bo[panel.bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(panel.bufnr, 0, -1, false, lines)
  vim.api.nvim_buf_clear_namespace(panel.bufnr, highlight_namespace, 0, -1)
  for _, highlight in ipairs(highlights) do
    vim.api.nvim_buf_add_highlight(
      panel.bufnr,
      highlight_namespace,
      highlight.group,
      highlight.line,
      highlight.start_col,
      highlight.end_col
    )
  end
  vim.bo[panel.bufnr].modifiable = false
  return #lines
end

---@param diagnostics? vim.Diagnostic[] Optional diagnostics, primarily useful for callers that
---already have a snapshot to display.
function M.refresh(diagnostics)
  if not panel.bufnr or not vim.api.nvim_buf_is_valid(panel.bufnr) then
    return
  end
  diagnostics = diagnostics or M.get()
  M.set_quickfix(diagnostics)
  set_panel_contents(diagnostics)
end

---@param diagnostics? vim.Diagnostic[] Optional diagnostics snapshot.
function M.open(diagnostics)
  local current_winid = vim.api.nvim_get_current_win()
  local winid = panel_window()
  if not winid then
    panel.source_winid = current_winid
    panel.bufnr = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(panel.bufnr, 'argon://diagnostics')
    configure_panel(panel.bufnr)
    vim.cmd('botright new')
    winid = vim.api.nvim_get_current_win()
    vim.api.nvim_win_set_buf(winid, panel.bufnr)
  elseif current_winid ~= winid then
    panel.source_winid = current_winid
    vim.api.nvim_set_current_win(winid)
  end

  diagnostics = diagnostics or M.get()
  M.set_quickfix(diagnostics)
  local line_count = set_panel_contents(diagnostics)
  vim.api.nvim_win_set_height(winid, math.min(20, math.max(6, line_count)))
end

local diagnostic_group = vim.api.nvim_create_augroup('argon_diagnostics_panel', { clear = true })
vim.api.nvim_create_autocmd('DiagnosticChanged', {
  group = diagnostic_group,
  callback = function()
    if panel_window() then
      vim.schedule(M.refresh)
    end
  end,
})

return M
