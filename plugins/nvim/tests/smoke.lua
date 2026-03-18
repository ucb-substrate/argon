local script_path = debug.getinfo(1, "S").source:sub(2)
local tests_dir = vim.fs.dirname(script_path)
local plugin_root = vim.fs.dirname(tests_dir)
local repo_root = vim.fs.dirname(vim.fs.dirname(plugin_root))

vim.opt.runtimepath:append(plugin_root)
package.path = table.concat({
  plugin_root .. "/lua/?.lua",
  plugin_root .. "/lua/?/init.lua",
  package.path,
}, ";")

local captured

vim.lsp.start = function(config, opts)
  captured = { config = config, opts = opts }
  return 1
end

vim.lsp.get_clients = function()
  return {}
end

vim.lsp.stop_client = function()
end

vim.g.argon = {
  argon_repo_path = "/tmp/argon",
  log = {
    level = "debug",
  },
}

local config = require("argon.config").config
assert(config.argon_repo_path == "/tmp/argon")
assert(config.log.level == "debug")

local argon = require("argon")

local workspace_file = repo_root .. "/examples/dimensions/lib.ar"
local workspace_buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(workspace_buf, workspace_file)
assert(argon.get_root_dir(workspace_buf) == repo_root .. "/examples/dimensions")

argon.start(workspace_buf)
assert(captured ~= nil)
assert(captured.opts.bufnr == workspace_buf)
assert(captured.config.name == "argon")
assert(captured.config.cmd[1] == "/tmp/argon/target/release/lang-server")
assert(captured.config.cmd_env.ARGON_LOG == "debug")
assert(captured.config.root_dir == repo_root .. "/examples/dimensions")

captured.config.on_init()
assert(vim.api.nvim_get_commands({})["Argon"] ~= nil)
captured.config.on_exit()
assert(vim.wait(100, function()
  return vim.api.nvim_get_commands({})["Argon"] == nil
end))

captured = nil
local scratch_dir = vim.fn.tempname()
vim.fn.mkdir(scratch_dir, "p")
local scratch_file = scratch_dir .. "/scratch.ar"
vim.fn.writefile({ "cell top() {}" }, scratch_file)
local scratch_buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(scratch_buf, scratch_file)

argon.start(scratch_buf)
assert(captured ~= nil)
assert(captured.config.root_dir == scratch_dir)
