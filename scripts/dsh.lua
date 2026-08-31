-- dsh.lua v2.1 — DSH 连接器(由内置 Rust 连接器 connectors/dsh.rs 迁移为外源脚本)
--
-- v2.1 修复(相对 v2):
--   * push 延迟:提问/审批只走 events.mux 推送,而 session.history 的大响应
--     (实测增量窗口就有 1.2MB+,纯 Lua 解码 ~0.3s/MB)会把单线程主循环占死
--     数秒——只在每轮末尾读一次 WS 时,push 帧在内核缓冲区里压过好几轮
--     轮询,实测提问进入 Attention 延迟十几秒(webui 不受影响,浏览器是
--     持续读自己 socket 的)。现在每个 HTTP 调用后都"抽水"读一次 WS,
--     push 延迟封顶在单次 HTTP+解码量级;ws_timeout_ms 默认 300→100,
--     让每次抽水的空转窗口足够小。
--   * mux 自愈周期动态化:有会话运行时 15s(提问没有 history 兜底,链路
--     半开时最长要等一个自愈周期),空闲 60s。
--
-- v2 修复(相对 v1):
--   * session/jobs 按 (sessionId, jobId) 记账:v1 每会话只留一槽,多 job
--     会话的 tool_started/ended 会错配,泄漏的 "job:" 工具名会把宠物钉在
--     Working;job 从快照消失时也补发 tool_ended 兜底;sessionId 为空的
--     jobs 帧直接丢弃(否则宿主里留下永不回收的幽灵会话)。
--   * 主循环调度改为挂钟时间(os.time):v1 按迭代计数,而每轮里两个 WS
--     读各可阻塞 ws_timeout_ms,mux 静默时每轮 ~700ms,history 实际轮询
--     间隔被拉长到 ~7s(配置值 1s)。时间基准下间隔不再受阻塞影响。
--
-- 行为与旧内置连接器一致:
--   session.list 轮询    → 基线会话状态(running / title / todo)
--   session.history 轮询 → 回合/工具/todo/实时文字流(seq 去重;会话首轮用大窗口
--                          重建当前状态,之后小窗口只发增量)——这是 DSH 会话事件
--                          的唯一来源(events.mux 不对外转发 session/event)
--   events.mux WS        → 审批 / 提问 / 任务(jobs) / 队列
--   events.host WS       → 会话 running 翻转
--   WS 重连后发 pet.pending_sync():服务端会重放当前仍在等待的审批/提问,
--                          清掉本地残留,避免宠物卡在 attention
--
-- 简化项(与旧内置一致可接受的差距):无 4 线程并行——本脚本单线程内
-- 交错 HTTP 轮询与 WS 读帧;WS 读超时小(ws_timeout_ms),近似实时。
--
-- 配置(args):
--   url            DSH 地址,默认 http://127.0.0.1:3080(env DSH_PET_URL 优先)
--   poll_ms        session.list 轮询间隔,默认 2000
--   history_ms     session.history 轮询间隔,默认 1000
--   ws_timeout_ms  WS 读超时,默认 100。该超时同时是握手与每次读帧窗口:
--                  主循环靠它在 HTTP 调用间隙"抽水"读取 mux 推送帧,窗口越
--                  小 push 延迟越低;远程高延迟链路可调大(如 300)
--   http_timeout_ms HTTP 超时,默认 5000
--   baseline_msgs  首轮基线窗口(消息条数),默认 200。会话历史可能非常大
--                  (超大会话可达几十 MB)——纯 Lua 解析大 JSON 较慢,若你的
--                  会话普遍很长,可调小(如 50)换取更快的首次状态重建
--   debug          true 时把每次历史增量/WS 帧都写日志(排查用)
--
-- 日志标记(在 hannis.log 里以 [lua:DSH] 开头):
--   "watching ..."            脚本已启动
--   "session.list ok=N"       第一次轮询成功,证明 HTTP 通
--   "events.mux connected"    审批/提问通道已连
--   "error: ..."              轮询/解析失败(会重试)

local cfg = pet.config() or {}
local args = cfg.args or {}
-- 接入口设置界面参数声明(键 | 标签 | 默认值):
--[hannis:set] url | DSH 地址(IP及端口) | http://127.0.0.1:3080
--[hannis:set] poll_ms | session.list 轮询间隔(ms) | 2000
--[hannis:set] history_ms | session.history 轮询间隔(ms) | 1000
--[hannis:set] ws_timeout_ms | WebSocket 读超时(ms) | 100

-- env DSH_PET_URL 优先于 args.url(与旧 config.dsh_url() 一致);沙箱下无 os
local env_url = (os and os.getenv and os.getenv("DSH_PET_URL")) or ""
local base = (env_url ~= "" and env_url or args.url or "http://127.0.0.1:3080"):gsub("/+$", "")
local poll_ms = tonumber(args.poll_ms) or 2000
local history_ms = tonumber(args.history_ms) or 1000
local ws_timeout = tonumber(args.ws_timeout_ms) or 100
local http_timeout = tonumber(args.http_timeout_ms) or 5000
local baseline_msgs = tonumber(args.baseline_msgs) or 200
local debug = args.debug == true

local HISTORY_SMALL = 2       -- 增量窗口(消息条数)
local HISTORY_BASELINE = baseline_msgs -- 首轮基线窗口:够回溯到回合 start 与未闭工具
local GRACE_SECS = 3          -- 会话从 running 掉出后再轮 history 的秒数(兜住最后 turn/end)
local NUL = "\0"              -- question 请求 key 分隔符(与内置 rpcId\0itemId 一致)

-- ---- 最小 JSON 解码器(只读;DSH 响应) ----

local function utf8_char(cp)
  if cp < 0x80 then return string.char(cp)
  elseif cp < 0x800 then return string.char(0xC0 + math.floor(cp / 0x40), 0x80 + cp % 0x40)
  elseif cp < 0x10000 then
    return string.char(0xE0 + math.floor(cp / 0x1000),
                       0x80 + math.floor(cp / 0x40) % 0x40, 0x80 + cp % 0x40)
  end
  return string.char(0xF0 + math.floor(cp / 0x40000),
                     0x80 + math.floor(cp / 0x1000) % 0x40,
                     0x80 + math.floor(cp / 0x40) % 0x40, 0x80 + cp % 0x40)
end

local function json_decode(s)
  local i, len = 1, #s
  local function skip() while i <= len and s:sub(i, i):match("%s") do i = i + 1 end end
  local function parse_string()
    i = i + 1
    local out = {}
    local esc = { ['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f",
                  n = "\n", r = "\r", t = "\t" }
    while i <= len do
      local c = s:sub(i, i)
      if c == '"' then i = i + 1; return table.concat(out) end
      if c == "\\" then
        local n = s:sub(i + 1, i + 1)
        if esc[n] then out[#out + 1] = esc[n]; i = i + 2
        elseif n == "u" then
          local cp = tonumber(s:sub(i + 2, i + 5), 16)
          i = i + 6
          if cp then
            if cp >= 0xD800 and cp <= 0xDBFF then
              local lo = tonumber(s:sub(i + 2, i + 5), 16)
              if lo and lo >= 0xDC00 and lo <= 0xDFFF then
                cp = 0x10000 + (cp - 0xD800) * 0x400 + (lo - 0xDC00)
                i = i + 6
              end
            end
            out[#out + 1] = utf8_char(cp)
          end
        else out[#out + 1] = n; i = i + 2 end
      else out[#out + 1] = c; i = i + 1 end
    end
    error("unterminated string")
  end
  local function parse_value()
    skip()
    local c = s:sub(i, i)
    if c == "" then error("unexpected end") end
    if c == "{" then
      i = i + 1
      local t = {}
      skip()
      if s:sub(i, i) == "}" then i = i + 1; return t end
      while true do
        skip()
        local k = parse_string()
        skip()
        if s:sub(i, i) ~= ":" then error("expected :") end
        i = i + 1
        t[k] = parse_value()
        skip()
        local d = s:sub(i, i)
        if d == "," then i = i + 1
        elseif d == "}" then i = i + 1; return t
        else error("expected , or }") end
      end
    elseif c == "[" then
      i = i + 1
      local t = {}
      skip()
      if s:sub(i, i) == "]" then i = i + 1; return t end
      while true do
        t[#t + 1] = parse_value()
        skip()
        local d = s:sub(i, i)
        if d == "," then i = i + 1
        elseif d == "]" then i = i + 1; return t
        else error("expected , or ]") end
      end
    elseif c == '"' then return parse_string()
    elseif s:sub(i, i + 3) == "true" then i = i + 4; return true
    elseif s:sub(i, i + 4) == "false" then i = i + 5; return false
    elseif s:sub(i, i + 3) == "null" then i = i + 4; return nil
    else
      local num = s:match("^-?%d+%.?%d*[eE]?[+-]?%d*", i)
      if not num then error("bad token at " .. i) end
      i = i + #num
      return tonumber(num)
    end
  end
  local v = parse_value()
  skip()
  return v
end

-- 把一帧里可能连续拼在一起的多个 JSON 对象拆开(按花括号配平;字符串内跳过)
local function split_json_objects(s)
  local out = {}
  local depth, start, in_str, esc = 0, nil, false, false
  for k = 1, #s do
    local c = s:sub(k, k)
    if in_str then
      if esc then esc = false
      elseif c == "\\" then esc = true
      elseif c == '"' then in_str = false end
    else
      if c == '"' then in_str = true
      elseif c == "{" then
        if depth == 0 then start = k end
        depth = depth + 1
      elseif c == "}" then
        depth = depth - 1
        if depth == 0 and start then
          out[#out + 1] = s:sub(start, k)
          start = nil
        end
      end
    end
  end
  return out
end

-- 最小 JSON 编码器(只用于构造请求 envelope)
local function json_encode(v)
  if v == nil then return "null" end
  local t = type(v)
  if t == "number" then
    if v % 1 == 0 then return string.format("%d", v) end
    return string.format("%g", v)
  elseif t == "boolean" then return v and "true" or "false"
  elseif t == "string" then
    return '"' .. v:gsub('[%z\1-\31\\"]', function(c)
      local m = { ['"'] = '\\"', ["\\"] = "\\\\", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }
      return m[c] or string.format("\\u%04x", c:byte())
    end) .. '"'
  elseif t == "table" then
    if #v > 0 then
      local parts = {}
      for i = 1, #v do parts[i] = json_encode(v[i]) end
      return "[" .. table.concat(parts, ",") .. "]"
    else
      local parts = {}
      for k, val in pairs(v) do
        parts[#parts + 1] = json_encode(k) .. ":" .. json_encode(val)
      end
      return "{" .. table.concat(parts, ",") .. "}"
    end
  end
  return "null"
end

-- ---- RPC ----
local rpc_seq = 0
local function rpc(method, payload, timeout)
  rpc_seq = rpc_seq + 1
  local body = json_encode({
    type = "client-request",
    rpcId = "pet-lua-" .. rpc_seq,
    method = method,
    payload = payload,
  })
  local status, resp = pet.http_post(base .. "/api/" .. method, body, timeout or http_timeout)
  if status ~= 200 then
    error("HTTP " .. status .. " from " .. method)
  end
  if debug then
    pet.log("info", method .. " resp len=" .. tostring(#resp))
  end
  local ok, v = pcall(json_decode, resp)
  if not ok then
    error(method .. " decode: " .. tostring(v))
  end
  if type(v) ~= "table" then
    error("bad response from " .. method)
  end
  return v
end

-- ---- session.list:基线快照 ----
local function list_sessions()
  local v = rpc("session.list", {})
  local result = v.result or {}
  if result.ok ~= true then
    error("session.list ok=false")
  end
  local items = (result.value and result.value.items) or result.items or {}
  local poll_items = {}
  local running = {}
  for _, it in ipairs(items) do
    local sid = it.sessionId or ""
    if sid ~= "" then
      local values = it.projections and it.projections.values or {}
      local title = values.title
      local todos = nil
      if type(values.todos) == "table" then
        todos = {}
        for _, t in ipairs(values.todos) do
          if t and t.content then
            table.insert(todos, { content = t.content, status = t.status or "pending" })
          end
        end
      end
      local running_flag = it.running == true
      table.insert(poll_items, {
        session_id = sid, running = running_flag, title = title, todos = todos,
      })
      if running_flag then running[sid] = true end
    end
  end
  return poll_items, running
end

-- ---- session.history:会话事件(seq 去重) ----
-- 状态:hist[sid] = { last_seq = <number|nil>, open_calls = {{callId,name},...}, recent = n }

local hist = {}
local hist_grace = {} -- sid -> 宽限截止(os.time):从 running 掉出后再轮 GRACE_SECS 秒

local function message_text(data)
  local out = ""
  local blocks = data.message and data.message.content
  if type(blocks) == "table" then
    for _, b in ipairs(blocks) do
      if b.type == "text" and b.text then out = out .. b.text end
    end
  end
  return out
end

local function todos_table(arr)
  local out = {}
  if type(arr) == "table" then
    for _, t in ipairs(arr) do
      if t and t.content then
        table.insert(out, { content = t.content, status = t.status or "pending" })
      end
    end
  end
  return out
end

-- DSH turn/end reason kind → pet.session_ended 的 reason 字符串
local REASON_MAP = {
  completed = "completed",
  error = "error",
  ["max-tokens"] = "max_tokens",
  aborted = "aborted",
  interrupted = "interrupted",
  blocked = "blocked",
}

local function apply_history(sid, entries, h)
  local seeded = h.last_seq ~= nil
  local last_seq = h.last_seq or 0
  local max_seq = last_seq
  local live_r, live_t = "", ""
  local function flush_live()
    if live_r ~= "" or live_t ~= "" then
      pet.live_text(sid, {
        reasoning = live_r ~= "" and live_r or nil,
        text = live_t ~= "" and live_t or nil,
      })
      live_r, live_t = "", ""
    end
  end

  if not seeded then
    -- ---- 基线:重建当前状态(不重放实时文字/回合结束) ----
    local open_turn = nil
    for _, entry in ipairs(entries) do
      local ev = entry and entry.event or {}
      local ev_seq = tonumber(ev.seq) or 0
      if ev_seq > max_seq then max_seq = ev_seq end
      if ev_seq <= last_seq then
        -- 已应用过
      else
        local data = ev.data or {}
        local t = ev.type or ""
        if t == "turn/start" then
          open_turn = tonumber(data.turn)
        elseif t == "turn/end" then
          if tonumber(data.turn) == open_turn then open_turn = nil end
        else
          -- 任何带 turn 的事件都提示当前回合(其 start 可能已滑出窗口)
          if open_turn == nil then open_turn = tonumber(data.turn) end
          if t == "tool/call" then
            -- 只认当前回合的工具(窗口内已结束回合的工具不算活着)
            local tool_turn = tonumber(data.turn)
            if open_turn == nil or tool_turn == nil or tool_turn == open_turn then
              local call_id = data.callId or ""
              local name = data.name or "tool"
              table.insert(h.open_calls, { call_id, name })
              pet.tool_started(sid, name, data.arguments)
            end
          elseif t == "tool/result" then
            local call_id = (data.message and data.message.source and data.message.source.callId) or ""
            for i = #h.open_calls, 1, -1 do
              local c = h.open_calls[i]
              if c[1] == call_id then
                pet.tool_ended(sid, c[2], data.error ~= nil)
                table.remove(h.open_calls, i)
                break
              end
            end
          elseif t == "todo/write" then
            pet.todo(sid, todos_table(data.todos))
          elseif t == "user/message" then
            local text = message_text(data)
            if text ~= "" then pet.user_message(sid, text) end
          end
          -- 注意:基线不重放审批事件——历史窗口里可能残留大量早已解决的
          -- approval/asked,补发会让宠物误入 Attention;待审批以 events.mux
          -- 的连接重放(mux-open replay)为准,它只含服务端仍挂起的请求
        end
      end
    end
    if open_turn ~= nil then
      pet.session_started(sid, open_turn)
    end
  else
    -- ---- 增量:只发 last_seq 之后的新事件 ----
    for _, entry in ipairs(entries) do
      local ev = entry and entry.event or {}
      local ev_seq = tonumber(ev.seq) or 0
      if ev_seq > max_seq then max_seq = ev_seq end
      if ev_seq > last_seq then
        local data = ev.data or {}
        local t = ev.type or ""
        if t == "turn/start" then
          pet.session_started(sid, tonumber(data.turn) or 0)
        elseif t == "turn/end" then
          local kind = (data.reason and data.reason.kind) or ""
          pet.session_ended(sid, tonumber(data.turn) or 0, REASON_MAP[kind] or "aborted")
          flush_live()
        elseif t == "tool/call" then
          local call_id = data.callId or ""
          local name = data.name or "tool"
          table.insert(h.open_calls, { call_id, name })
          pet.tool_started(sid, name, data.arguments)
          flush_live()
        elseif t == "tool/result" then
          local call_id = (data.message and data.message.source and data.message.source.callId) or ""
          local name = "tool"
          for i = #h.open_calls, 1, -1 do
            local c = h.open_calls[i]
            if c[1] == call_id then
              name = c[2]
              table.remove(h.open_calls, i)
              break
            end
          end
          pet.tool_ended(sid, name, data.error ~= nil)
        elseif t == "approval/asked" then
          -- 审批也走 history 增量:WS 僵尸/断连期间,1s 的 HTTP 轮询仍能
          -- 送达请求与解决,宠物 Attention 不再依赖推送通道
          local aid = data.id or ""
          if aid ~= "" then pet.approval_requested(aid, sid, data.toolName or "") end
        elseif t == "approval/decided" then
          local aid = data.id or ""
          if aid ~= "" then pet.approval_resolved(aid) end
        elseif t == "todo/write" then
          pet.todo(sid, todos_table(data.todos))
        elseif t == "assistant/chunk" then
          local chunk = data.chunk or {}
          local ctype = chunk.type or ""
          local text = chunk.text or ""
          if ctype == "reasoning-delta" then
            live_r = live_r .. text
          elseif ctype == "text-delta" then
            live_t = live_t .. text
          end
        elseif t == "user/message" then
          local text = message_text(data)
          if text ~= "" then pet.user_message(sid, text) end
          flush_live()
        elseif t == "assistant/message" or t == "step/end" then
          flush_live()
        end
      end
    end
    flush_live()
  end
  h.last_seq = max_seq
end

local function poll_history(sid)
  local h = hist[sid]
  if not h then
    h = { last_seq = nil, open_calls = {}, recent = 0 }
    hist[sid] = h
  end
  local max_msgs = h.last_seq and HISTORY_SMALL or HISTORY_BASELINE
  local v = rpc("session.history", { sessionId = sid, maxMessages = max_msgs })
  local result = v.result or {}
  if result.ok ~= true then
    -- 注意:不要在这里 log 整个响应——超大会话的响应可达几十 MB
    error("session.history ok=false")
  end
  local entries = (result.value and result.value.events) or {}
  apply_history(sid, entries, h)
end

-- 每 history_ms 对所有 running(+宽限)会话轮一次 history。
-- 宽限:会话从 running 掉出后仍轮 GRACE_SECS 秒,兜住最后的 turn/end;
-- 由主循环 list 成功时布防(hist_grace),这里只消费/过期。
-- pump_ws 为前置声明(定义在后文 mux/host 变量之后):每个 HTTP 调用后
-- 抽一次 WS,保证 push 帧(提问/审批)不被大响应解码压在缓冲区里。
local pump_ws
local function history_pass(running)
  local targets = {}
  local now_t = os.time()
  for sid in pairs(running) do targets[sid] = true end
  for sid, until_t in pairs(hist_grace) do
    if running[sid] then
      hist_grace[sid] = nil -- 重新跑起来了
      targets[sid] = true
    elseif now_t <= until_t then
      targets[sid] = true
    else
      hist_grace[sid] = nil -- 宽限用完:停止轮询(账目由宿主防抖/回收兜底)
    end
  end
  for sid in pairs(targets) do
    -- 每个会话独立轮询:一个会话出错不拖累其它会话
    local ok, err = pcall(poll_history, sid)
    if not ok then
      pet.log("error", "history " .. sid .. ": " .. tostring(err))
    end
    pump_ws() -- 大响应解码可能已占住循环数秒:先看看 push 帧到了没
  end
end

-- ---- events.mux / events.host ----
-- jobs 按 (sessionId, jobId) 记账:v1 每会话只留一槽,多 job 会话里后一个
-- job 会顶掉前一个的账,导致 "job:<id>" 的 tool_started 永远等不到配对的
-- tool_ended,宿主会一直停在 Working。值 = 最后见过的 status。
local jobs = {}

local function count_keys(tbl)
  local n = 0
  for _ in pairs(tbl) do n = n + 1 end
  return n
end

local function handle_mux(env)
  local payload = env.payload or {}
  local ftype = payload.type or ""
  local sid = payload.sessionId or ""
  if ftype == "session/jobs" then
    -- sessionId 为空的 jobs 帧无法归属会话:直接丢弃。旧版会照发 tool
    -- 事件,在宿主里留下 "script-0-" 幽灵会话(无 poll 基线,永不回收)
    if sid == "" then return end
    local list = payload.jobs or {}
    local seen = {}
    for _, j in ipairs(list) do
      local id = j.id or ""
      if id ~= "" then
        local key = sid .. NUL .. id
        local status = j.status or ""
        seen[key] = true
        local prev_status = jobs[key]
        local running = (status == "running" or status == "queued")
        local was_running = (prev_status == "running" or prev_status == "queued")
        if running and not was_running then
          pet.tool_started(sid, "job:" .. id, nil)
        elseif not running and was_running then
          pet.tool_ended(sid, "job:" .. id, status == "failed")
        end
        jobs[key] = status
      end
    end
    -- 从快照里消失的 job:主动收尾(正常应先收到终态帧;这里兜底,防止
    -- tool_started 没有配对 end 把宠物钉在 Working)
    for key, prev_status in pairs(jobs) do
      local jsid, jid = key:match("^(.-)" .. NUL .. "(.+)$")
      if jsid == sid and not seen[key] then
        if prev_status == "running" or prev_status == "queued" then
          pet.tool_ended(jsid, "job:" .. jid, false)
        end
        jobs[key] = nil
      end
    end
  elseif ftype == "approval/requested" then
    local aid = payload.approvalId or ""
    if aid ~= "" then pet.approval_requested(aid, sid, payload.toolName or "") end
  elseif ftype == "approval/resolved" then
    pet.approval_resolved(payload.approvalId or "")
  elseif ftype == "approval/asked" then
    local aid = payload.id or ""
    if aid ~= "" then pet.approval_requested(aid, sid, payload.toolName or "") end
  elseif ftype == "approval/decided" then
    pet.approval_resolved(payload.id or "")
  elseif ftype == "question/requested" then
    -- 与内置一致:按 envelope rpcId 建 key,因为 question/resolved 只带 rpcId
    local rpc_id = env.rpcId or ""
    for _, q in ipairs(payload.questions or {}) do
      pet.question(rpc_id .. NUL .. (q.id or ""), sid, q.question or "")
    end
  elseif ftype == "question/resolved" then
    pet.answer(payload.questionRpcId or "")
  elseif ftype == "question/asked" then
    local rpc_id = env.rpcId or ""
    for _, q in ipairs(payload.questions or {}) do
      pet.question(rpc_id .. NUL .. (q.id or ""), sid, q.question or "")
    end
    if payload.question then
      pet.question(rpc_id .. NUL .. (payload.id or ""), sid, payload.question or "")
    end
  elseif ftype == "question/decided" then
    pet.answer(payload.questionRpcId or payload.id or "")
  elseif ftype == "session/event" then
    -- 新版 events.mux 把会话事件统一包成 session/event 帧,审批/提问等
    -- 关键事件在 payload.event 里(旧版是顶层 approval/requested 直发帧)。
    local ev = payload.event or {}
    local etype = ev.type or ""
    local data = ev.data or {}
    if etype == "approval/asked" then
      local aid = data.id or ""
      if aid ~= "" then pet.approval_requested(aid, sid, data.toolName or "") end
    elseif etype == "approval/decided" then
      pet.approval_resolved(data.id or "")
    elseif etype == "question/asked" then
      local rpc_id = env.rpcId or ""
      for _, q in ipairs(data.questions or {}) do
        pet.question(rpc_id .. NUL .. (q.id or ""), sid, q.question or "")
      end
      if data.question then
        pet.question(rpc_id .. NUL .. (data.id or ""), sid, data.question or "")
      end
    elseif etype == "question/decided" or etype == "question/resolved" then
      pet.answer(data.questionRpcId or data.id or "")
    end
  end
end

local function handle_host(env)
  local payload = env.payload or {}
  local ftype = payload.type or ""
  if ftype == "host/session-status" then
    local sid = payload.sessionId or ""
    if sid ~= "" then
      pet.session_status(sid, payload.running == true)
    end
  end
end

-- ---- 健康与主循环 ----
local list_ok = false
local mux_alive = false
local host_alive = false
local healthy = false
local function update_health()
  local h = list_ok or mux_alive or host_alive
  if h ~= healthy then
    healthy = h
    pet.health(h)
  end
end

local tick = 100
-- 调度基于挂钟时间(os.time,秒级)而非迭代计数:每轮里两个 WS 读各可阻塞
-- ws_timeout_ms(默认 300ms),mux 静默时一轮 ~700ms —— 旧版按 i%N 计数会把
-- history 的实际轮询间隔拉长到配置值的数倍(默认配置下 ~7s)。秒级取整略粗
-- 于配置值,但保证间隔下限不受 WS 阻塞影响。
local list_every_s = math.max(1, math.floor(poll_ms / 1000))
local hist_every_s = math.max(1, math.floor(history_ms / 1000))
local ws_retry_s = 5 -- WS 重连退避(秒)
-- 周期性自愈重连:events.mux/host 是纯下行通道(客户端发任何帧都会被
-- 服务端以 1008 关闭,服务端也不发 ping),链路半开时(对端卡顿/网络抖动/
-- 休眠唤醒)脚本永远察觉不到,审批/提问帧就永远漏掉。定期主动重连,
-- 服务端会在连接建立时重放当前全部 pending 审批/提问,自愈漏单。
local ws_reconnect_s = 60
local next_list_at, next_hist_at = 0, 0
local next_mux_try_at, next_host_try_at = 0, 0
local mux_connected_at, host_connected_at = 0, 0

pet.log("info", "dsh.lua v2.1 watching " .. base .. " (poll " .. poll_ms .. "ms, history " .. history_ms .. "ms, ws " .. ws_timeout .. "ms)")

local running_cache = {}
local mux, host = nil, nil
local list_fail_streak = 0 -- 连续 session.list 失败次数(服务器卡顿指标)

-- 读一帧 mux 并处理;返回 "frame" | "timeout" | "dead"
local function read_mux_once()
  local frame = pet.ws_read(mux)
  if frame == false then return "dead" end
  if frame == nil then return "timeout" end
  for _, part in ipairs(split_json_objects(frame)) do
    local ok, env = pcall(json_decode, part)
    if ok and type(env) == "table" then
      local ok2, err2 = pcall(handle_mux, env)
      if not ok2 then pet.log("error", "mux frame: " .. tostring(err2)) end
    end
  end
  return "frame"
end

-- 读一帧 host 并处理;返回 "frame" | "timeout" | "dead"
local function read_host_once()
  local frame = pet.ws_read(host)
  if frame == false then return "dead" end
  if frame == nil then return "timeout" end
  for _, part in ipairs(split_json_objects(frame)) do
    local ok, env = pcall(json_decode, part)
    if ok and type(env) == "table" then
      local ok2, err2 = pcall(handle_host, env)
      if not ok2 then pet.log("error", "host frame: " .. tostring(err2)) end
    end
  end
  return "frame"
end

-- 快速抽水:把 mux/host 已到达的帧各读一帧并处理。提问/审批只走 mux 推送,
-- 而 session.history 的大响应解码动辄数秒(实测增量窗口 1.2MB+;纯 Lua 解码
-- ~0.3s/MB)——只在每轮末尾读 WS 的话,push 帧会在内核缓冲区里压过好几轮
-- 轮询,实测提问进入 Attention 延迟十几秒。每个 HTTP 调用后抽一次,push
-- 延迟就封顶在"单次 HTTP + 解码"量级。有帧时 read 立即返回,无帧时空转
-- ws_timeout_ms(默认 100ms)×2。
pump_ws = function()
  if mux ~= nil and read_mux_once() == "dead" then
    mux_alive = false
    pcall(pet.ws_close, mux)
    mux = nil
    next_mux_try_at = os.time() + ws_retry_s
    pet.log("info", "events.mux disconnected, reconnecting")
  end
  if host ~= nil and read_host_once() == "dead" then
    host_alive = false
    pcall(pet.ws_close, host)
    host = nil
    next_host_try_at = os.time() + ws_retry_s
    pet.log("info", "events.host disconnected, reconnecting")
  end
end

-- mux 自愈周期:有会话运行时 15s,空闲 60s(见 ws_reconnect_s 注释)
local function reconnect_interval_s()
  if next(running_cache) then return 15 end
  return ws_reconnect_s
end

while true do
  local now_t = os.time()

  -- 1) session.list 基线
  if now_t >= next_list_at then
    next_list_at = now_t + list_every_s
    local ok, items, running = pcall(list_sessions)
    if ok then
      list_ok = true
      -- 服务器从卡顿中恢复(session.list 曾失败、现在成功):WS 大概率已被
      -- 拖死,立即自愈重连不等 60s 周期 —— 重连即重放 pending 审批/提问
      if list_fail_streak > 0 then
        list_fail_streak = 0
        if mux ~= nil or host ~= nil then
          pet.log("info", "server recovered after stall -> ws self-heal")
        end
        if mux ~= nil then
          pcall(pet.ws_close, mux)
          mux = nil
        end
        if host ~= nil then
          pcall(pet.ws_close, host)
          host = nil
        end
      end
      -- 从 running 掉出(但仍在 hist 里)的会话:进入宽限,再轮 GRACE_SECS
      -- 秒兜住最后的 turn/end;只在"上一拍还在跑"的下降沿布防,避免已结束
      -- 的会话被反复拉回来轮询
      for sid in pairs(hist) do
        if running[sid] then
          hist_grace[sid] = nil
        elseif running_cache[sid] and hist_grace[sid] == nil then
          hist_grace[sid] = now_t + GRACE_SECS
        end
      end
      running_cache = running -- 只有 running 的会话才需要 history 轮询
      pet.poll(items)
      pump_ws() -- list 响应也不小(百级会话):先抽水再继续
      if debug then pet.log("info", "session.list ok, running=" .. count_keys(running_cache)) end
    else
      list_ok = false
      list_fail_streak = list_fail_streak + 1
      pet.log("error", "session.list: " .. tostring(items))
    end
    update_health()
  end

  -- 2) session.history 增量
  if now_t >= next_hist_at then
    next_hist_at = now_t + hist_every_s
    local ok, err = pcall(history_pass, running_cache)
    if not ok then
      pet.log("error", "history: " .. tostring(err))
    end
  end

  -- 3) events.mux:自愈重连(有会话运行时 15s,空闲 60s)+ 常规读帧
  if mux ~= nil and now_t - mux_connected_at >= reconnect_interval_s() then
    pcall(pet.ws_close, mux) -- 自愈重连:见 ws_reconnect_s 注释
    mux = nil
    pet.log("info", "events.mux periodic reconnect (self-heal)")
  end
  if mux == nil then
    if now_t >= next_mux_try_at then
      local ok, h = pcall(pet.ws, base, "/api/events.mux", ws_timeout)
      if ok then
        mux = h
        mux_alive = true
        mux_connected_at = now_t
        pet.pending_sync() -- 服务端重放当前 pending;清本地残留
        pet.log("info", "events.mux connected")
      else
        next_mux_try_at = now_t + ws_retry_s
        pet.log("info", "events.mux connect failed: " .. tostring(h))
      end
    end
  elseif read_mux_once() == "dead" then
    mux_alive = false
    pcall(pet.ws_close, mux)
    mux = nil
    next_mux_try_at = now_t + ws_retry_s
    pet.log("info", "events.mux disconnected, reconnecting")
  end

  -- 4) events.host:同 mux(下行通道无保活,定期自愈)
  if host ~= nil and now_t - host_connected_at >= reconnect_interval_s() then
    pcall(pet.ws_close, host)
    host = nil
    pet.log("info", "events.host periodic reconnect (self-heal)")
  end
  if host == nil then
    if now_t >= next_host_try_at then
      local ok, h = pcall(pet.ws, base, "/api/events.host", ws_timeout)
      if ok then
        host = h
        host_alive = true
        host_connected_at = now_t
        pet.log("info", "events.host connected")
      else
        next_host_try_at = now_t + ws_retry_s
        pet.log("info", "events.host connect failed: " .. tostring(h))
      end
    end
  elseif read_host_once() == "dead" then
    host_alive = false
    pcall(pet.ws_close, host)
    host = nil
    next_host_try_at = now_t + ws_retry_s
    pet.log("info", "events.host disconnected, reconnecting")
  end

  update_health()
  pet.wait(tick)
end
