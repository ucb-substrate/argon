local M = {}

M.init = function(argon_repo_dir)
    vim.opt.runtimepath:append(argon_repo_dir..'/plugins/nvim')
    vim.cmd([[autocmd BufRead,BufNewFile *.ar setfiletype argon]])
    vim.lsp.config('argon_lsp', {
    cmd = { argon_repo_dir..'/target/debug/lsp-server' },
    filetypes = { 'argon' },
    root_dir = function(bufnr, on_dir)
        local fname = vim.api.nvim_buf_get_name(bufnr)
        on_dir(vim.fs.dirname(fname))
    end
    })
    vim.lsp.enable('argon_lsp')
    local function handler(err)
    if err then
        vim.notify(tostring(err), vim.log.levels.ERROR)
        return
    end
    vim.notify('Argon GUI started')
    end
    vim.api.nvim_create_user_command('ArgonLsp', function()
    require('argon_lsp.client').buf_request(0, "custom/startGui", nil, handler)
    end, { desc = 'Starts the Argon GUI' })
end

return M
