local repo_root = assert(vim.env.ARGON_REPO_ROOT, "ARGON_REPO_ROOT must be set")
local plugin_root = repo_root .. "/plugins/nvim"

vim.opt.runtimepath:append(plugin_root)
vim.g.argon = {
  argon_repo_path = repo_root,
  log = {
    level = "debug",
  },
}

local notifications = {}
vim.notify = function(msg, level)
  table.insert(notifications, { msg = msg, level = level })
end

local started = {}
local stopped = nil
local fake_clients = {}

vim.lsp = vim.lsp or {}
vim.lsp.start = function(config, opts)
  table.insert(started, { config = config, opts = opts })
  if type(config.on_init) == "function" then
    config.on_init()
  end
  return #started
end
vim.lsp.stop_client = function(clients)
  stopped = clients
end
vim.lsp.get_clients = function(filter)
  local ret = {}
  for _, client in ipairs(fake_clients) do
    if not filter or not filter.name or client.name == filter.name then
      if not filter or filter.bufnr == nil or client.bufnr == filter.bufnr then
        table.insert(ret, client)
      end
    end
  end
  return ret
end

local function assert_eq(actual, expected, context)
  if actual ~= expected then
    error(string.format("%s: expected %s, got %s", context, vim.inspect(expected), vim.inspect(actual)))
  end
end

local function write_file(path, lines)
  vim.fn.mkdir(vim.fn.fnamemodify(path, ":h"), "p")
  vim.fn.writefile(lines, path)
end

local tmp_dir = repo_root .. "/plugins/nvim/tests/tmp"
vim.fn.delete(tmp_dir, "rf")

local workspace = repo_root .. "/plugins/nvim/tests/tmp/workspace"
local nested_file = workspace .. "/src/nested.ar"
write_file(workspace .. "/lib.ar", { "cell test() {}" })
write_file(nested_file, { "cell nested() {}" })

local outside_dir = repo_root .. "/plugins/nvim/tests/tmp/outside"
local outside_file = outside_dir .. "/scratch.ar"
write_file(outside_file, { "cell outside() {}" })

local argon = require("argon")

local workspace_buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(workspace_buf, nested_file)

assert_eq(argon.get_root_dir(workspace_buf), workspace, "workspace root detection")
argon.start(workspace_buf)

local start_cfg = assert(started[1], "expected workspace start config")
assert_eq(start_cfg.opts.bufnr, workspace_buf, "workspace bufnr")
assert_eq(start_cfg.config.root_dir, workspace, "workspace root")
assert_eq(start_cfg.config.cmd[1], repo_root .. "/target/release/lang-server", "lang-server command")
assert_eq(start_cfg.config.cmd_env.ARGON_LOG, "debug", "log env")
assert_eq(vim.fn.exists(":Argon"), 2, "Argon command creation")

fake_clients = {
  {
    id = 7,
    name = "argon",
    bufnr = workspace_buf,
    config = { root_dir = workspace },
    is_stopped = function()
      return true
    end,
  },
}

local clients = argon.stop(workspace_buf)
assert_eq(#clients, 1, "stop returned clients")
assert_eq(stopped[1].id, 7, "stop_client received argon client")

assert(type(start_cfg.config.on_exit) == "function", "on_exit should be set")
start_cfg.config.on_exit()
vim.wait(100, function()
  return vim.fn.exists(":Argon") == 0
end)
assert_eq(vim.fn.exists(":Argon"), 0, "Argon command deletion")

local outside_buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(outside_buf, outside_file)
argon.start(outside_buf)

local fallback_cfg = assert(started[2], "expected fallback start config")
assert_eq(fallback_cfg.config.root_dir, outside_dir, "fallback root")
assert(notifications[1] and notifications[1].msg:match("Could not detect workspace"), "expected workspace warning")
