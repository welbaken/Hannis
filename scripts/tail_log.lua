-- tail_log.lua — 通用日志监控示例(开放接口)
--
-- 用法:在 config.json 的 "scripts" 里注册,例如:
--   {
--     "name": "MyGame",
--     "file": "scripts/tail_log.lua",
--     "poll_ms": 1000,
--     "args": {
--       "log": "D:\\MyGame\\game.log",      -- 要监控的日志路径
--       "session": "game",                  -- 会话 id(自动加 script-N- 前缀)
--       "work": "开始",                     -- 行内出现 → 开始工作(working)
--       "done": "完成",                     -- 行内出现 → 完成(done)
--       "fail": "失败",                     -- 行内出现 → 出错(fail)
--       "connect": "连接",                  -- 行内出现 → 连接中(thinking)
--       "tail": 500                         -- 启动时从文件末尾偏移量起读
--     }
--   }
--
-- 原理:像 tail -f 一样跟踪文件新追加的内容,把匹配关键词的行翻译成宠物状态。
-- 关键词可任意组合(留空则不启用);session/tool 名称由 args 控制。

local cfg = pet.config()
local args = cfg.args or {}
local log_path = args.log or "app.log"
local session = args.session or "app"
local tool = args.work or "task"
local tail = args.tail or 500
local stream = args.stream ~= false -- 非映射行也作为信息流显示

-- 打开日志;打不开则标记该源不健康(宠物整体不受影响)
local f = io.open(log_path, "r")
if not f then
  pet.log("error", "cannot open " .. log_path)
  pet.health(false)
  return
end

-- 只关心新内容:定位到文件末尾再往前一点(避免错过刚写入的行)
local size = f:seek("end")
local from = math.max(0, size - tail)
f:seek("set", from)
if from > 0 then f:read("*l") end -- 丢弃被截断的半行

pet.health(true)
pet.log("info", "watching " .. log_path)
local started = false

while true do
  local line = f:read("*l")
  if line then
    if stream then pet.live_text(session, { text = line }) end
    if args.fail and line:find(args.fail, 1, true) then
      if started then pet.tool_ended(session, tool, true) end
      pet.session_ended(session, 1, "error")
      started = false
    elseif args.done and line:find(args.done, 1, true) then
      if started then pet.tool_ended(session, tool, false) end
      pet.session_ended(session, 1, "completed")
      started = false
    elseif args.connect and line:find(args.connect, 1, true) then
      if not started then
        pet.session_started(session, 1)
        started = true
      end
    elseif args.work and line:find(args.work, 1, true) then
      if not started then
        pet.session_started(session, 1)
        started = true
      end
      pet.tool_started(session, tool, nil)
    end
  else
    -- 没有新行:等待(可中断的切片睡眠,不影响程序退出)
    pet.wait(cfg.poll_ms or 1000)
  end
end
