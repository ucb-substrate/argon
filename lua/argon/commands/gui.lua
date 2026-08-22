local M = {}

local client = require('argon.client')

function M.start_gui()
    client.buf_request(0, "custom/startGui", nil, client.print_error)
end

function M.open_cell(cell)
    local client_found = client.buf_request(0, "custom/openCell", {
        cell = cell
    }, client.print_error)
    if not client_found then
        vim.notify('No Argon language server is attached to this buffer', vim.log.levels.ERROR)
    end
end

function M.instantiate(cell)
    local bufnr = vim.api.nvim_get_current_buf()
    local client_found = false
    for _, lsp_client in ipairs(client.get_active_argon_lsp_clients(bufnr)) do
        local params = vim.lsp.util.make_position_params(0, lsp_client.offset_encoding)
        params.cell = cell
        lsp_client:request('custom/inst', params, client.print_error, bufnr)
        client_found = true
    end
    if not client_found then
        vim.notify('No Argon language server is attached to this buffer', vim.log.levels.ERROR)
    end
end

function M.set(kv)
    client.buf_request(0, "custom/set", {
        kv = kv
    }, client.print_error)
end

return M
