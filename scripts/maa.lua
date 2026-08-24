-- maa.lua — MAA(明日方舟小助手)监控(由内置 Rust 连接器迁移为外源脚本)
--
-- 行为与旧内置连接器一致:
--   正在连接模拟器                     → thinking(气泡流式文字)
--   开始任务: XXX                     → working(⚙ XXX)
--   "Idle: false to true (called from ProcTaskChainMsg)" → done(整条链只庆祝一次;
--                                             链内每个任务的"完成任务"行不单独触发)
--   已停止                            → fail
--   …资深干员…                        → attention;MAA 不等待人工确认,约
--                                     attention_ms 秒后自动消除
-- "Main windows log clear." / 日志截断 = 运行边界(中性结束);启动时扫描全部
-- 已有日志恢复"进行中的链"(仅 30 分钟内)。
--
-- 配置(args):log=日志路径;attention_ms=资深干员提示时长(默认 3000);
--           stream=true 时把任务期间的用户可见日志行([TaskQueueViewModel])
--           组装为信息流显示在气泡里(如「理智作战」期间的 开始行动/掉落统计 等),
--           直到下一次 done/fail/attention。设 stream=false 关闭。

local cfg = pet.config() or {}
local args = cfg.args or {}
-- 接入口设置界面参数声明(键 | 标签 | 默认值):
--[hannis:set] log | MAA 日志路径 | D:\MeoAssistantArknights\debug\gui.log
--[hannis:set] attention_ms | attention 判定阈值(ms) | 3000
--[hannis:set] stream | 信息流开关(true/false) | true
local path = args.log or "D:\\MeoAssistantArknights\\debug\\gui.log"
local poll = cfg.poll_ms or 1000
local attention_ms = args.attention_ms or 3000
local stream = args.stream ~= false -- 信息流开关(默认开)
local FRESH_SECS = 30 * 60 -- 启动恢复只认 30 分钟内的未完成链

-- ---- 日志行解析(与内置连接器同一套) ----
local function line_message(line)
  -- [ts][LVL][Source] <N> msg —— 取第 3 个 ] 之后的部分
  local i = 0
  for _ = 1, 3 do
    local _, b = line:find("%]", i + 1)
    if not b then return line end
    i = b
  end
  local rest = line:sub(i + 1):gsub("^%s+", "")
  local _, e, num = rest:find("^<(%d+)>")
  if num then rest = rest:sub(e + 1):gsub("^%s+", "") end
  return rest
end

local function task_name(msg)
  local i = msg:find("任务", 1, true)
  if not i then return "任务" end
  -- 注意:find 返回字节位置,"任务" 占 6 字节
  local rest = msg:sub(i + #"任务"):gsub("^%s*:?%s*", ""):gsub("%s+$", "")
  if rest == "" then return "任务" end
  return rest
end

-- 行时间戳 "MM-DD HH:MM" 与 unix 秒(仅取整秒)
local function line_time(line)
  local date = line:match("^%[([%d%-]+ [%d%:]+)%.?%d*%]")
  if not date then return nil, nil end
  local disp = date:match("(%d%d%-%d%d %d%d:%d%d)")
  local y, mo, d, h, mi, se = date:match("(%d+)%-(%d+)%-(%d+) (%d+):(%d+):(%d+)")
  if not y then return nil, nil end
  local t = os.time({ year = tonumber(y), month = tonumber(mo), day = tonumber(d),
                      hour = tonumber(h), min = tonumber(mi), sec = tonumber(se) })
  return t, disp
end

local function classify(line)
  local msg = line_message(line)
  if line:find("正在连接模拟器", 1, true) then
    return "connect"
  elseif line:find("开始任务", 1, true) then
    return "start", task_name(msg)
  elseif line:find("Idle: false to true (called from ProcTaskChainMsg)", 1, true) then
    return "done"
  elseif msg == "已停止" or msg:sub(1, 4) == "已停止 " then
    return "stop"
  elseif line:find("连接失败", 1, true) then
    -- 连接失败 → fail(与已停止同语义)
    return "stop"
  elseif line:find("Main windows log clear.", 1, true) then
    return "clear"
  elseif line:find("资深干员", 1, true) then
    return "senior", msg
  end
  return nil
end

-- ---- 链状态(与内置连接器一致) ----
local session_id = nil
local turn = 0
local connect_open = false
local task = nil
local last_task = nil
local run_ended = false
local pending_q = nil
local pending_at = 0
local q_seq = 0
local last_line = nil
-- 消息流分隔标记:消息之间补 "\n"(GUI 端 text 累积);新链复位
local need_nl = false

local function resolve_q()
  if pending_q then
    pet.answer(pending_q)
    pending_q = nil
  end
end

local function start_chain(label)
  session_id = "maa-" .. tostring(os.time())
  turn = 1
  connect_open = false
  task = nil
  last_task = nil
  run_ended = false
  pending_q = nil
  need_nl = false -- 新链:消息流从头开始,第一条前不加换行
  pet.session_started(session_id, turn)
end

local function on_connect()
  -- 进行中的链内重连(模拟器重连)忽略;否则是新一轮运行
  if session_id and (connect_open or task) and not run_ended then
    return
  end
  resolve_q()
  start_chain(os.date("%m-%d %H:%M"))
  connect_open = true
  pet.live_text(session_id, { reasoning = "正在连接模拟器……" })
end

local function on_task_start(name)
  resolve_q()
  if session_id and not run_ended then
    if connect_open then
      connect_open = false
    elseif not task then
      turn = turn + 1
      pet.session_started(session_id, turn)
    end
    if task then
      pet.tool_ended(session_id, task, false)
    end
    task = name
    pet.tool_started(session_id, task, nil)
  else
    start_chain(os.date("%m-%d %H:%M"))
    task = name
    pet.tool_started(session_id, task, nil)
  end
end

local function on_task_done()
  if session_id and not run_ended then
    if task then
      last_task = task
      pet.tool_ended(session_id, task, false)
      task = nil
    end
    connect_open = false
    pet.session_ended(session_id, turn, "completed")
    resolve_q()
  end
end

local function on_stopped()
  if session_id and not run_ended then
    if task then
      last_task = task
      pet.tool_ended(session_id, task, false)
      task = nil
    end
    connect_open = false
    run_ended = true
    pet.session_ended(session_id, turn, "error")
    resolve_q()
  end
end

local function on_clear()
  -- 运行边界:进行中的链中性结束(不清除已结束的链)
  if session_id then
    if (connect_open or task) and not run_ended then
      if task then
        pet.tool_ended(session_id, task, false)
        task = nil
      end
      connect_open = false
      pet.session_ended(session_id, turn, "aborted")
    end
    run_ended = true
    resolve_q()
  end
end

local function on_senior(msg)
  if not session_id or run_ended then
    start_chain(os.date("%m-%d %H:%M"))
  end
  if pending_q then
    pet.answer(pending_q)
  end
  q_seq = q_seq + 1
  pending_q = "maa-q" .. tostring(q_seq)
  pending_at = os.time()
  pet.question(pending_q, session_id, msg)
end

local function handle(line)
  if line == last_line then return end -- 连续重复行去重
  last_line = line
  local kind, a = classify(line)
  if kind == "connect" then on_connect()
  elseif kind == "start" then on_task_start(a)
  elseif kind == "done" then on_task_done()
  elseif kind == "stop" then on_stopped()
  elseif kind == "clear" then on_clear()
  elseif kind == "senior" then on_senior(a)
  end
end

-- ---- 信息流:按轮组装「用户可见」日志消息(TaskQueueViewModel 行 + 裸续行),
-- 轮末统一发射给 pet.live_text(气泡尾部滚动显示;done/fail/attention 后由
-- 状态机自动清空)。
local stream_buf = nil
local function flush_stream()
  if stream_buf and session_id then
    local sep = need_nl and "\n" or ""
    pet.live_text(session_id, { text = sep .. stream_buf })
    need_nl = true
  end
  stream_buf = nil
end

-- 打开并扫描(恢复进行中的链 + 定位到日志末尾) ----
local f = io.open(path, "r")
if not f then
  pet.log("error", "cannot open " .. path)
  pet.health(false)
  return
end
pet.health(true)

-- 逐行扫描:记录最后一个映射行;同时把句柄推到文件末尾待后续 tail
local last_kind, last_name, last_ts
while true do
  local line = f:read("*l")
  if not line then break end
  local t = line_time(line)
  local kind, name = classify(line)
  if kind == "connect" or kind == "start" then
    last_kind, last_name, last_ts = kind, name, t
  elseif kind == "done" or kind == "stop" or kind == "clear" then
    last_kind = kind -- 已结束的链:不恢复
    last_name, last_ts = nil
  end
end

if last_kind == "start" or last_kind == "connect" then
  if last_ts and os.time() - last_ts <= FRESH_SECS then
    start_chain(os.date("%m-%d %H:%M"))
    if last_kind == "start" then
      task = last_name
      pet.tool_started(session_id, task, nil)
    else
      connect_open = true
      pet.live_text(session_id, { reasoning = "正在连接模拟器……" })
    end
  end
end
pet.log("info", "watching " .. path)
local last_size = f and f:seek("end") or 0

while true do
  -- 截断检测(清空/删除后重建):重置为新文件
  local size = 0
  local ok_size, s2 = nil, io.open(path, "r")
  if s2 then size = s2:seek("end"); s2:close() end
  if s2 == nil then
    pet.health(false)
    pet.log("error", "log disappeared, retrying")
    pet.wait(poll)
    goto continue
  end
  pet.health(true)
  if size < last_size then
    on_clear()
    f:close()
    f = io.open(path, "r")
  end
  last_size = size

  -- 读取新行(组装消息流 + 驱动状态机)
  while true do
    local line = f:read("*l")
    if not line then break end
    if line ~= "" then
      if line:match("^%[") then
        -- 带时间戳 = 新消息(先发射上一条)
        flush_stream()
        local kind = classify(line)
        if kind == nil and stream and line:find("[TaskQueueViewModel]", 1, true)
           and not line:find("完成任务", 1, true) then
          stream_buf = line_message(line)
        end
      elseif stream_buf then
        -- 裸续行(如「公招识别结果:」后的标签列表)接续上一条消息
        stream_buf = stream_buf .. "\n" .. line
      end
      handle(line)
    end
  end
  flush_stream()

  -- 资深干员 attention 自动消除(不等待人工确认,约 attention_ms 秒)
  if pending_q and os.time() - pending_at >= math.ceil(attention_ms / 1000) then
    resolve_q()
  end

  ::continue::
  pet.wait(poll)
end
