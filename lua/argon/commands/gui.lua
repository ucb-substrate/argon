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
    local client_found = client.buf_request(0, "custom/inst", {
        cell = cell
    }, client.print_error)
    if not client_found then
        vim.notify('No Argon language server is attached to this buffer', vim.log.levels.ERROR)
    end
end

function M.reload_config()
    client.buf_request(0, "custom/reloadConfig", nil, client.print_error)
end

function M.set_config(key, value)
    client.buf_request(0, "custom/setConfig", {
        key = key,
        value = value,
    }, client.print_error)
end

function M.save_config(path)
    if path then
        path = vim.fn.fnamemodify(vim.fn.expand(path), ':p')
    end
    client.buf_request(0, "custom/saveConfig", {
        path = path,
    }, client.print_error)
end

return M
