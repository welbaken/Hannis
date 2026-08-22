-- process_watch.lua — 进程监控示例(开放接口)
--
-- 用法:config.json "scripts" 注册:
--   {
--     "name": "GameWatcher",
--     "file": "scripts/process_watch.lua",
--     "poll_ms": 2000,
--     "args": { "process": "game.exe" }
--   }
--
-- 原理:周期性调用系统 tasklist 检查进程是否存在;出现 → working,
-- 消失 → done。这是最简单的"某个程序在不在干活"的接入方式。

local cfg = pet.config()
local args = cfg.args or {}
local proc = args.process or "game.exe"
local session = args.session or "proc"

pet.health(true)
pet.log("info", "watching process " .. proc)
local running = false

while true do
  -- 注意:io.popen 在沙箱模式下不可用(sandbox 会移除 io)
  local pipe = io.popen('tasklist /FI "IMAGENAME eq ' .. proc .. '" /NH 2>nul')
  local out = pipe and pipe:read("*a") or ""
  if pipe then pipe:close() end

  local is_running = out:find(proc, 1, true) ~= nil
  if is_running and not running then
    pet.session_started(session, 1)
    pet.tool_started(session, proc, nil)
    running = true
  elseif not is_running and running then
    pet.tool_ended(session, proc, false)
    pet.session_ended(session, 1, "completed")
    running = false
  end
  pet.wait(cfg.poll_ms or 2000)
end
