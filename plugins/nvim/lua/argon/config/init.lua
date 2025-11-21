---@mod argon.config plugin configuration
---
---@brief [[
---
---argon is a filetype plugin, and does not need
---a `setup` function to work.
---
---To configure argon, set the variable `vim.g.argon`,
---which is a |argon.Opts| table, in your neovim configuration.
---
---Notes:
---
--- - `vim.g.argon` can also be a function that returns a |argon.Opts| table.
---
---@brief ]]

---@class argon.Opts
---
---The path of the local Argon repository (for development purposes).
---@field argon_repo_path? string

local config = {}

---@type argon.Opts | fun():argon.Opts | nil
vim.g.argon = vim.g.argon

local argon = vim.g.argon or {}
local argon_opts = type(argon) == 'function' and argon() or argon

---Wrapper around |vim.fn.exepath()| that returns the binary if no path is found.
---@param binary string
---@return string the full path to the executable or `binary` if no path is found.
---@see vim.fn.exepath()
local function exepath_or_binary(binary)
  local exe_path = vim.fn.exepath(binary)
  return #exe_path > 0 and exe_path or binary
end

---@class argon.config.Config
local Config = {
    --- Defaults to `nil`, which means argon will not use a local development repo as source.
    ---@type nil | string
    argon_repo_path = nil,
    log = {
        --- Log level following [`RUST_LOG`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/index.html#filtering-events-with-environment-variables) syntax.
        --- Defaults to `nil`.
        ---@type nil | string
        level = nil
    },
}

---@type argon.config.Config
config.config = vim.tbl_deep_extend('force', {}, Config, argon_opts)

return config

