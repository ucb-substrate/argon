local M = {}

local client = require('argon_lsp.client')

function M.start_gui()
    client.buf_request(0, "custom/startGui", nil, client.print_error)
end

function M.open_cell(cell)
    bufnr = vim.api.nvim_get_current_buf()
    local bufname = vim.api.nvim_buf_get_name(bufnr)
    client.buf_request(0, "custom/openCell", {
        file = bufname,
        cell = cell
    }, client.print_error)
end

function M.set(kv)
    client.buf_request(0, "custom/set", {
        kv = kv
    }, client.print_error)
end

return M
