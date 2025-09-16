---@mod argon_lsp.config plugin configuration
---
---@brief [[
---
---argon_lsp is a filetype plugin, and does not need
---a `setup` function to work.
---
---To configure argon_lsp, set the variable `vim.g.argon_lsp`,
---which is a |argon_lsp.Opts| table, in your neovim configuration.
---
---Notes:
---
--- - `vim.g.argon_lsp` can also be a function that returns a |argon_lsp.Opts| table.
---
---@brief ]]

---@class argon_lsp.Opts
---
---The path of the local Argon repository (for development purposes).
---@field argon_repo_path? string

local ArgonLspConfig

---@type argon_lsp.Opts | fun():argon_lsp.Opts | nil
vim.g.argon_lsp = vim.g.argon_lsp

local argon_lsp = vim.g.argon_lsp or {}
local argon_lsp_opts = type(argon_lsp) == 'function' and argon_lsp() or argon_lsp

---Wrapper around |vim.fn.exepath()| that returns the binary if no path is found.
---@param binary string
---@return string the full path to the executable or `binary` if no path is found.
---@see vim.fn.exepath()
local function exepath_or_binary(binary)
  local exe_path = vim.fn.exepath(binary)
  return #exe_path > 0 and exe_path or binary
end

---@class argon_lsp.Config
local ArgonLspDefaultConfig = {
    --- Defaults to `nil`, which means argon_lsp will not use a local development repo as source.
    ---@type nil | string
    argon_repo_path = nil,
}

---@type argon_lsp.Config
ArgonLspConfig = vim.tbl_deep_extend('force', {}, ArgonLspDefaultConfig, argon_lsp_opts)

return ArgonLspConfig

