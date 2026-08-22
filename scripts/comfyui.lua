-- comfyui.lua v2 — ComfyUI 出图队列监控(外源脚本)
--
-- 行为:
--   /queue 有任务在跑 → working;任务完成 → done;出错 → fail
--   queue_pending → 气泡"队列中还有 N 个任务"
--   /queue 不可达 → 该源不健康(错误会写进 hannis.log)
--
-- 配置(args):
--   url        服务地址,默认 http://127.0.0.1:8188(尾斜杠会被去掉)
--   timeout_ms HTTP 超时,默认 5000
--   debug      true 时每次队列变化都写日志(排查用)
--
-- 日志标记(在 hannis.log 里以 [lua:ComfyUI] 开头):
--   "watching ..."                脚本已启动(版本 v2)
--   "queue snapshot: running=N pending=M"   第一次轮询成功,证明 HTTP 通
--   "run start <id>"              探测到任务开始生成
--   "run end <id> -> completed|error|aborted"  任务收尾
--   "error: ..."                  轮询失败(网络/JSON 等),会重试

local cfg = pet.config() or {}
local args = cfg.args or {}
local base = (args.url or "http://127.0.0.1:8188"):gsub("/+$", "")
local timeout_ms = args.timeout_ms or 5000
local poll = cfg.poll_ms or 2000
local debug = args.debug == true

-- ---- 最小 JSON 解码器(只读;ComfyUI 响应) ----

-- Unicode 码点 → UTF-8(JSON 的 \uXXXX 可能是 CJK 等 >0xFF 的码点,
-- string.char 只接受 0-255,必须自行编码;同时处理代理对)
local function utf8_char(cp)
  if cp < 0x80 then
    return string.char(cp)
  elseif cp < 0x800 then
    return string.char(0xC0 + math.floor(cp / 0x40), 0x80 + cp % 0x40)
  elseif cp < 0x10000 then
    return string.char(
      0xE0 + math.floor(cp / 0x1000),
      0x80 + math.floor(cp / 0x40) % 0x40,
      0x80 + cp % 0x40
    )
  end
  return string.char(
    0xF0 + math.floor(cp / 0x40000),
    0x80 + math.floor(cp / 0x1000) % 0x40,
    0x80 + math.floor(cp / 0x40) % 0x40,
    0x80 + cp % 0x40
  )
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
            -- 代理对:高位代理 + 低位代理组合成 astral 码点
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

-- 取队列条目的 prompt_id,兼容两种 /queue 格式:
--   新格式(部分 fork):{ prompt_id = "...", ... }
--   官方格式(当前 master 也是):[number, prompt_id, prompt, extra_data, outputs]
local function item_pid(item)
  if type(item) ~= "table" then return nil end
  local pid = item.prompt_id
  if pid ~= nil then return pid end
  if type(item[2]) == "string" then return item[2] end
  return nil
end

-- 条目键名一览(报错时打印,便于适配未知格式)
local function item_keys(item)
  local ks = {}
  for k, _ in pairs(item) do ks[#ks + 1] = tostring(k) end
  return table.concat(ks, ",")
end

-- ---- 轮询主循环 ----
local function get(p)
  local status, body = pet.http(base .. p, timeout_ms)
  if status ~= 200 then
    error("HTTP " .. status .. " from " .. base .. p)
  end
  return json_decode(body)
end

-- 当前执行中的 prompt;终态追查最多重试轮数
local current = nil
local queued = -1
local max_hist_tries = 12

pet.health(true)
pet.log("info", "comfyui v2.1 watching " .. base .. " (poll " .. poll .. "ms)")

local first = true
while true do
  local ok, q = pcall(get, "/queue")
  if not ok then
    pet.health(false)
    pet.log("error", tostring(q))
    pet.wait(poll)
  else
    pet.health(true)
    local running = q.queue_running or {}
    local r = running and running[1]
    if first then
      first = false
      pet.log("info", "queue snapshot: running=" .. #running .. " pending=" .. #(q.queue_pending or {}))
    end

    if r then
      local pid = item_pid(r)
      if not pid then
        -- 响应形状跟两种已知格式都对不上:打印键名,继续轮询
        pet.log("error", "unexpected /queue item shape (no prompt_id); keys=" .. item_keys(r))
      elseif current ~= pid then
        if current then
          -- 上一个任务没带终态就换了 → 中性收尾
          pet.tool_ended(current, "run", false)
          pet.session_ended(current, 1, "aborted")
          pet.log("info", "run end " .. current .. " -> aborted (preempted)")
        end
        current = pid
        pet.session_started(pid, 1)
        pet.tool_started(pid, "run", nil)
        pet.log("info", "run start " .. pid)
      end
    elseif current then
      -- 执行结束:查 /history/<id> 的终态;条目可能晚一两个轮询才出现,
      -- 重试而不是立刻误判 aborted。期间若新任务顶上,旧任务按 preempted 收尾。
      local final = nil
      for tries = 1, max_hist_tries do
        local okh, h = pcall(get, "/history/" .. current)
        if okh then
          local st = h[current] and h[current].status and h[current].status.status_str
          if st == "success" then
            final = "completed"
          elseif st == "error" then
            final = "error"
          elseif st then
            final = "aborted"
          end
        end
        if final then break end
        local okq2, q2 = pcall(get, "/queue")
        local r2 = okq2 and q2.queue_running and q2.queue_running[1]
        local p2 = r2 and item_pid(r2)
        if p2 and p2 ~= current then
          final = "aborted" -- 新任务顶上,旧任务没有终态
          pet.log("info", "run end " .. current .. " -> aborted (preempted)")
          break
        end
        pet.wait(poll)
      end
      if final == nil then
        pet.log("error", "no history for " .. current .. " after " .. max_hist_tries .. " polls -> aborted")
        final = "aborted"
      end
      pet.tool_ended(current, "run", final == "error")
      pet.session_ended(current, 1, final)
      if final ~= "aborted" or debug then
        pet.log("info", "run end " .. current .. " -> " .. final)
      end
      current = nil
    end

    local n = #(q.queue_pending or {})
    if n ~= queued then
      queued = n
      pet.queue(n)
      if debug then pet.log("info", "queue pending = " .. n) end
    end
  end
  pet.wait(poll)
end
