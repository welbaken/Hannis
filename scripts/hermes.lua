-- hermes.lua v2 — Hermes 连接器(由内置 Rust 连接器 connectors/hermes.rs 迁移为外源脚本)
--
-- v2 修复(相对 v1):WAL 副本回退只复制一次,之后永远读启动时刻的快照,
--   状态会永久冻结。现在每 60s 重试直连(恢复即切回),仍失败则按原库大小
--   变化/超时重新复制,保证轮询数据跟着 Hermes 前进。
--
-- 行为与旧内置连接器一致:只读轮询 Hermes 的 SQLite 数据库(sessions/messages)。
--   sessions 表:ended_at=NULL 且 last_active 新鲜(10 分钟内)→ running;翻转发
--               session_started/session_ended(含 end_reason 映射)+ session_status
--   messages 表:每会话取最新一行(1s 轮询≈准流式);同一行增长 → 只发增量
--               (思考先行、正文随后);assistant 行带 tool_calls → 工具事件;
--               finish_reason=tool_calls 且含 clarify 调用 → 提问(attention),
--               用户回答(结果行落库)或会话结束 → 自动恢复
--   WAL/shm 权限问题:直接只读打开失败时,复制 db+wal+shm 到临时目录再读(与内置
--   的 open_with_fallback 一致);沙箱模式无 io/os,跳过该回退
--
-- 配置(args):
--   db_path         数据库路径;留空=自动解析:
--                   env HERMES_WEB_UI_HOME(若设置) → %USERPROFILE%\.hermes-web-ui\hermes-web-ui.db
--   poll_ms_active  有活跃会话时的轮询间隔(ms),默认 1000——决定思考文本刷新粒度
--   poll_ms_idle    空闲轮询间隔(ms),默认 2000
--   debug           true 时把每次消息变化都写日志(排查用)
--
-- 日志标记(在 hannis.log 里以 [lua:Hermes] 开头):
--   "watching <db>"       脚本已启动
--   "error: ..."          轮询失败(会重试;db 打不开则该源不健康)

local cfg = pet.config() or {}
local args = cfg.args or {}
-- 接入口设置界面参数声明(键 | 标签 | 默认值):
--[hannis:set] db_path | Hermes 数据库路径 | 
--[hannis:set] poll_ms_active | 活跃会话轮询间隔(ms) | 1000
--[hannis:set] poll_ms_idle | 空闲轮询间隔(ms) | 2000

local poll_ms_active = tonumber(args.poll_ms_active) or 1000
local poll_ms_idle = tonumber(args.poll_ms_idle) or 2000
local debug = args.debug == true

-- env HERMES_WEB_UI_HOME > %USERPROFILE%/.hermes-web-ui/hermes-web-ui.db
-- (与旧 config.hermes_db_path() 一致;沙箱下无 os)
local function default_db()
  if not (os and os.getenv) then return nil end
  local hh = os.getenv("HERMES_WEB_UI_HOME")
  if hh and hh ~= "" then return hh .. "/hermes-web-ui.db" end
  local user = os.getenv("USERPROFILE") or os.getenv("HOME")
  if user and user ~= "" then return user .. "/.hermes-web-ui/hermes-web-ui.db" end
  return nil
end

local db_path = (args.db_path and args.db_path ~= "") and args.db_path or default_db()

-- ---- 最小 JSON 解码器(只读;messages.tool_calls 是 JSON 字符串) ----

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

-- ---- 数据库打开(含 WAL 副本回退) ----
local function copy_file(src, dst)
  local f = io.open(src, "rb")
  if not f then return false end
  local data = f:read("*a")
  f:close()
  local g = io.open(dst, "wb")
  if not g then return false end
  g:write(data)
  g:close()
  return true
end

local active_db
local using_copy = false -- true = 在读临时副本(原库直连失败,如 WAL/shm 权限)
local copy_size, copy_at = nil, 0 -- 副本对应的原库大小/复制时刻(刷新判据)

-- 原库当前大小(刷新判据;沙箱下无 io → nil,不刷新)
local function orig_size()
  if not (io and os) then return nil end
  local f = io.open(db_path, "rb")
  if not f then return nil end
  local s = f:seek("end")
  f:close()
  return s
end

local function copy_to_tmp()
  if not (io and os and os.getenv) then return nil end
  local tmpbase = os.getenv("TEMP") or os.getenv("TMP") or "/tmp"
  local dir = tmpbase .. "/dshpet-hermes-" .. tostring(os.time()) .. "-" .. tostring(math.random(10000, 99999))
  local okm = os.execute('mkdir "' .. dir .. '"')
  if not okm then return nil end
  local copied = false
  for _, suf in ipairs({ "", "-wal", "-shm" }) do
    if copy_file(db_path .. suf, dir .. "/hermes-web-ui.db" .. suf) then copied = true end
  end
  if not copied then return nil end
  return dir .. "/hermes-web-ui.db"
end

local function open_db()
  -- 直接只读打开
  local ok, _ = pcall(pet.sqlite, db_path, "SELECT 1")
  if ok then return db_path, false end
  -- 回退:复制 db+wal+shm 到临时目录再读(仅非沙箱)
  local p = copy_to_tmp()
  if p then return p, true end
  return nil, false
end

-- ---- 会话轮询 ----
local FRESH_SECS = 10 * 60 -- ended_at NULL 但 last_active 超过 10 分钟 → 不算 running
local LOOKBACK_SECS = 2 * 3600

local prev = {} -- sid -> 增量记忆(与内置 PrevSession 同构)

local function pending_clarify(tool_calls, finish_reason)
  if not (finish_reason or ""):find("tool_calls", 1, true) then return nil end
  if not tool_calls then return nil end
  local ok, v = pcall(json_decode, tool_calls)
  if not ok or type(v) ~= "table" then return nil end
  for _, call in ipairs(v) do
    local fn = call and call["function"] -- "function" 是 Lua 关键字,必须用 [] 访问
    if fn and fn.name == "clarify" then
      local call_id = call.id or ""
      if call_id ~= "" then
        local text = ""
        local ok2, args = pcall(json_decode, fn.arguments or "")
        if ok2 and type(args) == "table" then
          if args.question then text = text .. args.question end
          if type(args.choices) == "table" then
            local ch = {}
            for _, c in ipairs(args.choices) do
              if type(c) == "string" then ch[#ch + 1] = c end
            end
            if #ch > 0 then
              local choices_s = table.concat(ch, " / ")
              if text == "" then text = choices_s
              else text = text .. "（" .. choices_s .. "）" end
            end
          end
        end
        if text == "" or text:gsub("%s", "") == "" then text = "等待你确认…" end
        return { call_id, text }
      end
    end
  end
  return nil
end

-- 工具行 content JSON 里的 output 字段 → 短预览
local function truncate_chars(s, n)
  local i, count = 1, 0
  while i <= #s and count < n do
    local b = s:byte(i)
    local len = 1
    if b and b >= 0xF0 then len = 4 elseif b and b >= 0xE0 then len = 3 elseif b and b >= 0xC0 then len = 2 end
    i = i + len
    count = count + 1
  end
  if i <= #s then return s:sub(1, i - 1) end
  return s
end

local function tool_content_preview(content)
  local out = content
  local ok, v = pcall(json_decode, content)
  if ok and type(v) == "table" and type(v.output) == "string" then out = v.output end
  if out == "" then return nil end
  return truncate_chars(out, 160)
end

local function poll_messages(id, p)
  local rows = pet.sqlite(active_db, [[
    SELECT id, role, tool_name, content, display_content, reasoning_content, reasoning,
           tool_calls, finish_reason, tool_call_id
    FROM messages WHERE session_id = ? ORDER BY id DESC LIMIT 1]], { id })
  if #rows == 0 then return end
  local r = rows[1]
  local msg_id = r.id
  local role = r.role
  local tool_name = r.tool_name
  local tool_calls = r.tool_calls
  local finish_reason = r.finish_reason
  local tool_call_id = r.tool_call_id
  local text = r.display_content or r.content or ""
  local reasoning_text = r.reasoning_content or r.reasoning or ""

  local changed = (msg_id ~= p.last_msg_id)
      or (p.last_content ~= text)
      or (p.last_reasoning ~= reasoning_text)
  if not changed then return end
  local new_msg = (msg_id ~= p.last_msg_id)
  -- 先取旧快照再覆盖:同 id 增长的增量要对着旧文本来 diff(顺序错了 delta 恒为空)
  local prev_t = p.last_content or ""
  local prev_r = p.last_reasoning or ""
  p.last_msg_id = msg_id
  p.last_content = text
  p.last_reasoning = reasoning_text

  -- ---- 交互提问检测(clarify) ----
  local clarify = pending_clarify(tool_calls, finish_reason)
  if p.pending_question then
    local pid = p.pending_question[1]
    local answered = (role == "tool") and (tool_call_id == pid)
    local superseded = not (clarify and clarify[1] == pid)
    if answered or superseded then
      pet.answer(pid)
      p.pending_question = nil
    end
  end
  if not p.pending_question then
    if clarify then
      p.pending_question = clarify
      pet.question(clarify[1], id, clarify[2])
    end
  end

  if role == "tool" then
    if tool_name and tool_name ~= "" then
      -- clarify 的答案行是用户回复记录,不是干活:上面已处理
      if tool_name == "clarify" then return end
      if p.pending_tool ~= tool_name then
        if p.pending_tool then pet.tool_ended(id, p.pending_tool, false) end
        p.pending_tool = tool_name
        pet.tool_started(id, tool_name, tool_content_preview(text))
      end
    end
    return
  end
  -- 非工具消息:先关掉挂着的工具,再发实时文字
  if p.pending_tool then
    pet.tool_ended(id, p.pending_tool, false)
    p.pending_tool = nil
  end
  if role == "user" and text ~= "" then
    pet.user_message(id, text)
    return
  end
  if not new_msg then
    -- 同一行增长(流式写入):只发增量
    local dtext = text
    if prev_t ~= "" and text:sub(1, #prev_t) == prev_t then dtext = text:sub(#prev_t + 1) end
    local dreason = reasoning_text
    if prev_r ~= "" and reasoning_text:sub(1, #prev_r) == prev_r then dreason = reasoning_text:sub(#prev_r + 1) end
    if dtext ~= "" or dreason ~= "" then
      pet.live_text(id, {
        reasoning = dreason ~= "" and dreason or nil,
        text = dtext ~= "" and dtext or nil,
      })
    end
    return
  end
  pet.live_text(id, {
    reasoning = reasoning_text ~= "" and reasoning_text or nil,
    text = text ~= "" and text or nil,
  })
end

-- 一轮轮询;返回是否有活跃会话(决定下次轮询间隔)
local function poll_once()
  local now_s = os.time()
  local rows = pet.sqlite(active_db, [[
    SELECT id, title, ended_at, end_reason, last_active
    FROM sessions
    WHERE ended_at IS NULL OR last_active > ?
    ORDER BY last_active DESC]], { now_s - LOOKBACK_SECS })

  local items = {}
  local seen = {}
  local any_running = false

  for _, row in ipairs(rows) do
    local id = row.id
    local title = row.title
    local ended_at = row.ended_at
    local end_reason = row.end_reason
    local last_active = row.last_active
    local fresh = last_active ~= nil and (now_s - last_active) < FRESH_SECS
    local running = (ended_at == nil) and fresh
    seen[id] = true

    local p = prev[id] or {
      running = false, turn = 0, last_msg_id = 0,
      last_content = nil, last_reasoning = nil,
      pending_tool = nil, pending_question = nil, title = nil,
    }
    prev[id] = p
    p.title = title

    if running and not p.running then
      p.running = true
      p.turn = p.turn + 1
      p.pending_tool = nil
      pet.session_started(id, p.turn)
      if debug then pet.log("info", "session started " .. id) end
    elseif not running and p.running then
      p.running = false
      local reason = "aborted"
      if end_reason == "complete" or end_reason == "completed" then
        reason = "completed"
      elseif end_reason == "error" or end_reason == "failed" then
        reason = "error"
      end
      if p.pending_tool then
        pet.tool_ended(id, p.pending_tool, false)
        p.pending_tool = nil
      end
      if p.pending_question then
        pet.answer(p.pending_question[1])
        p.pending_question = nil
      end
      pet.session_ended(id, p.turn, reason)
      pet.session_status(id, false)
      if debug then pet.log("info", "session ended " .. id .. " (" .. reason .. ")") end
    end

    if running then
      any_running = true
      poll_messages(id, p)
    end

    table.insert(items, { session_id = id, running = running, title = title, todos = nil })
  end

  -- 会话从窗口消失(结束超过 2h 或删除)且还在 running → 中性收尾
  for id, p in pairs(prev) do
    if not seen[id] then
      if p.running then
        if p.pending_tool then pet.tool_ended(id, p.pending_tool, false) end
        if p.pending_question then pet.answer(p.pending_question[1]) end
        pet.session_ended(id, p.turn, "aborted")
        pet.session_status(id, false)
        if debug then pet.log("info", "session vanished " .. id .. " -> aborted") end
      end
      prev[id] = nil
    end
  end

  pet.poll(items)
  return any_running
end

-- ---- 主循环 ----
active_db, using_copy = open_db()
if not active_db then
  pet.health(false)
  pet.log("error", "hermes db unavailable: " .. (db_path or "(unresolved)"))
  return -- 源下线(日志可见原因)
end

pet.log("info", "hermes.lua v2 watching " .. active_db .. " (active " .. poll_ms_active .. "ms, idle " .. poll_ms_idle .. "ms)")

-- 副本保鲜:临时副本是启动时刻的快照,一直读它状态会永久冻结。每 60s 试一次
-- 直连(权限恢复即切回);仍不行则按"原库大小变化 或 超 10 分钟"重新复制,
-- 保证轮询读到的数据跟着 Hermes 前进。
local next_direct_check = os.time() + 60
if using_copy then
  copy_size = orig_size()
  copy_at = os.time()
  pet.log("info", "direct open failed, using tmp copy (will refresh)")
end

local healthy = false
while true do
  if using_copy and os.time() >= next_direct_check then
    next_direct_check = os.time() + 60
    local okd = pcall(pet.sqlite, db_path, "SELECT 1")
    if okd then
      active_db = db_path
      using_copy = false
      pet.log("info", "direct db open recovered; dropping tmp copy")
    else
      local size = orig_size()
      if size ~= copy_size or os.time() - copy_at >= 600 then
        local p = copy_to_tmp()
        if p then
          active_db = p
          copy_size, copy_at = size, os.time()
          pet.log("info", "refreshed wal copy -> " .. p)
        end
      end
    end
  end
  local ok, any = pcall(poll_once)
  if ok then
    if not healthy then
      healthy = true
      pet.health(true)
    end
    pet.wait(any and poll_ms_active or poll_ms_idle)
  else
    if healthy then
      healthy = false
      pet.health(false)
    end
    pet.log("error", "poll: " .. tostring(any))
    pet.wait(poll_ms_idle)
  end
end
