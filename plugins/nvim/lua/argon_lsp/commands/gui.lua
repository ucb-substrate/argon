local M = {}

local client = require('argon_lsp.client')

function M.start_gui()
    client.buf_request(0, "custom/startGui", nil, client.print_error)
end

function M.open_cell(cell)
    client.buf_request(0, "custom/openCell", {
        cell = cell
    }, client.print_error)
end

function M.set(kv)
    client.buf_request(0, "custom/set", {
        kv = kv
    }, client.print_error)
end

return M
