---@mod argon.commands

---@class argon.Commands
local M = {}

local argon_cmd_name = 'Argon'
local gui = require('argon.commands.gui')

---@class argon.command_tbl
---@field impl fun(args: string[], opts: vim.api.keyset.user_command) The command implementation
---@field complete? fun(subcmd_arg_lead: string): string[] Command completions callback, taking the lead of the subcommand's arguments
---@field bang? boolean Whether this command supports a bang!

---@type argon.command_tbl[]
local argon_command_tbl = {
  gui = {
    impl = function(_, opts)
      gui.start_gui()
    end,
  },
  openCell = {
    impl = function(args, opts)
      gui.open_cell(table.concat(args, " "))
    end,
  },
  set = {
    impl = function(args, opts)
      gui.set(table.concat(args, " "))
    end,
  },
  log = {
      impl = function(args, opts)
          -- TODO: allow configuration of log file name.
          vim.cmd('tabnew ~/.local/state/argon/lang-server.log')
      end
  }
}

---@param command_tbl argon.command_tbl
---@param opts table
---@see vim.api.nvim_create_user_command
local function run_command(command_tbl, cmd_name, opts)
  local fargs = opts.fargs
  local cmd = fargs[1]
  local args = #fargs > 1 and vim.list_slice(fargs, 2, #fargs) or {}
  local command = command_tbl[cmd]
  if type(command) ~= 'table' or type(command.impl) ~= 'function' then
    vim.notify(cmd_name .. ': Unknown subcommand: ' .. cmd, vim.log.levels.ERROR)
    return
  end
  command.impl(args, opts)
end

---@param opts table
---@see vim.api.nvim_create_user_command
local function argon(opts)
  run_command(argon_command_tbl, argon_cmd_name, opts)
end

---@generic K, V
---@param predicate fun(V):boolean
---@param tbl table<K, V>
---@return K[]
local function tbl_keys_by_value_filter(predicate, tbl)
  local ret = {}
  for k, v in pairs(tbl) do
    if predicate(v) then
      ret[k] = v
    end
  end
  return vim.tbl_keys(ret)
end

---Create the `:Argon` command
function M.create_argon_command()
  vim.api.nvim_create_user_command(argon_cmd_name, argon, {
    nargs = '+',
    range = true,
    bang = true,
    desc = 'Interacts with the Argon LSP client',
    complete = function(arg_lead, cmdline, _)
      local commands = cmdline:match("^['<,'>]*" .. argon_cmd_name .. '!') ~= nil
          -- bang!
          and tbl_keys_by_value_filter(function(command)
            return command.bang == true
          end, argon_command_tbl)
        or vim.tbl_keys(argon_command_tbl)
      local subcmd, subcmd_arg_lead = cmdline:match("^['<,'>]*" .. argon_cmd_name .. '[!]*%s(%S+)%s(.*)$')
      if subcmd and subcmd_arg_lead and argon_command_tbl[subcmd] and argon_command_tbl[subcmd].complete then
        return argon_command_tbl[subcmd].complete(subcmd_arg_lead)
      end
      if cmdline:match("^['<,'>]*" .. argon_cmd_name .. '[!]*%s+%w*$') then
        return vim.tbl_filter(function(command)
          return command:find(arg_lead) ~= nil
        end, commands)
      end
    end,
  })
end

--- Delete the `:Argon` command
function M.delete_argon_command()
  if vim.cmd[argon_cmd_name] then
    pcall(vim.api.nvim_del_user_command, argon_cmd_name)
  end
end

return M

