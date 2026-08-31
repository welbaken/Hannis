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
local healthy = true
local unknown_streak = 0

local function set_health(h)
  if h ~= healthy then
    healthy = h
    pet.health(h)
  end
end

while true do
  -- 注意:io.popen 在沙箱模式下不可用(sandbox 会移除 io)
  local ok_cmd, pipe = pcall(io.popen, 'tasklist /FI "IMAGENAME eq ' .. proc .. '" /NH 2>nul')
  local out = ""
  if ok_cmd and pipe then
    out = pipe:read("*a") or ""
    pipe:close()
  end

  local is_running = out:find(proc, 1, true) ~= nil
  -- 区分"查询失败"与"进程不在":tasklist 正常运行时总有输出(命中行或
  -- 本地化的 "INFO: 没有运行的任务…"),输出为空说明命令本身失败了。
  -- 失败时按未知处理,不能当成"进程消失"误报 done。
  local known = is_running or out ~= ""
  if not known then
    unknown_streak = unknown_streak + 1
    set_health(false)
    -- 连续多轮都失败:按中性收尾(源标记不健康;后续恢复会重新探测),
    -- 避免会话账目悬挂,也避免一次抖动就误报
    if unknown_streak >= 3 and running then
      pet.tool_ended(session, proc, false)
      pet.session_ended(session, 1, "aborted")
      running = false
    end
  else
    unknown_streak = 0
    set_health(true)
    if is_running and not running then
      pet.session_started(session, 1)
      pet.tool_started(session, proc, nil)
      running = true
    elseif not is_running and running then
      pet.tool_ended(session, proc, false)
      pet.session_ended(session, 1, "completed")
      running = false
    end
  end
  pet.wait(cfg.poll_ms or 2000)
end
