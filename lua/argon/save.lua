local M = {}
local reported_workspace_modified = {}

local function client_buffers(client_id)
    return vim.lsp.get_buffers_by_client_id
        and vim.lsp.get_buffers_by_client_id(client_id)
        or vim.api.nvim_list_bufs()
end

local function is_file_backed_argon_buffer(bufnr, client_id)
    return vim.api.nvim_buf_is_valid(bufnr)
        and vim.lsp.buf_is_attached(bufnr, client_id)
        and vim.bo[bufnr].buftype == ''
        and vim.api.nvim_buf_get_name(bufnr) ~= ''
        and vim.bo[bufnr].filetype == 'argon'
end

---Whether an analyzer client has any modified, file-backed Argon buffers.
---@param client_id number
---@return boolean
function M.workspace_modified(client_id)
    for _, bufnr in ipairs(client_buffers(client_id)) do
        if is_file_backed_argon_buffer(bufnr, client_id) and vim.bo[bufnr].modified then
            return true
        end
    end
    return false
end

---Send Neovim's authoritative workspace modified state to the analyzer.
---@param client_id number
function M.notify_workspace_modified(client_id)
    local lsp_client = vim.lsp.get_client_by_id(client_id)
    if not lsp_client or lsp_client.name ~= 'argon' then
        return
    end
    local modified = M.workspace_modified(client_id)
    if reported_workspace_modified[client_id] == modified then
        return
    end
    reported_workspace_modified[client_id] = modified
    lsp_client:request(
        'custom/workspaceModified',
        { modified = modified },
        function(err)
            if err then
                if reported_workspace_modified[client_id] == modified then
                    reported_workspace_modified[client_id] = nil
                end
                vim.notify('argon: Could not update GUI save state: ' .. tostring(err), vim.log.levels.ERROR)
            end
        end,
        0
    )
end

---Write every modified, file-backed Argon buffer attached to one LSP client.
---@param client_id number
function M.save_modified_buffers(client_id)
    for _, bufnr in ipairs(client_buffers(client_id)) do
        if is_file_backed_argon_buffer(bufnr, client_id) and vim.bo[bufnr].modified then
            vim.api.nvim_buf_call(bufnr, function()
                vim.cmd('update')
            end)
        end
    end
end

return M
