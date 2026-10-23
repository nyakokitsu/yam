-- Self test for Station Lite 2 (plum).
--
-- Runs a fixed sequence so a human can watch the hardware and confirm each
-- step. Every step holds long enough to be seen.
--
-- Notes about this platform:
--   * ring = 3 zones, each RGB: plum:{red,green,blue}:ring1..3
--   * clock = 36 segments: A..H x positions 0..3, plus DEGREE, DH, DM, DL
--   * alice.clock.segments() writes the whole frame: any segment not listed is
--     cleared, so every frame below is built in full.
--   * DH + DL are the colon; DM is the separate middle segment (the bar).
--   * The sandbox exposes only table/string/math/utf8, so `print` does not
--     exist and must not be called.

local HOLD = 1500 -- ms per visible step

-- Digit font, same table the stock firmware uses.
local FONT = {
  ["0"] = {"A", "B", "C", "D", "E", "F"},
  ["1"] = {"B", "C"},
  ["2"] = {"A", "B", "D", "E", "G"},
  ["3"] = {"A", "B", "C", "D", "G"},
  ["4"] = {"B", "C", "F", "G"},
  ["5"] = {"A", "C", "D", "F", "G"},
  ["6"] = {"A", "C", "D", "E", "F", "G"},
  ["7"] = {"A", "B", "C"},
  ["8"] = {"A", "B", "C", "D", "E", "F", "G"},
  ["9"] = {"A", "B", "C", "D", "F", "G"},
  ["-"] = {"G"},
  [" "] = {},
}

-- ring_colour fills all zones with one colour.
local function ring_colour(hex)
  local frame = {}
  for i = 1, device.led_count do
    frame[i] = hex
  end
  alice.set(frame)
end

-- clock_frame renders four characters plus colon and bar level.
local function clock_frame(text, brightness, bar)
  local frame = {}
  for pos = 0, 3 do
    local char = string.sub(text, pos + 1, pos + 1)
    local parts = FONT[char] or FONT[" "]
    for _, part in ipairs(parts) do
      frame[part .. pos] = brightness
    end
  end
  frame.DH = brightness
  frame.DL = brightness
  frame.DM = bar or 0
  return frame
end

-- 1. Ring: each primary colour in turn -------------------------------------
ring_colour("#FF0000")
alice.sleep(HOLD)
ring_colour("#00FF00")
alice.sleep(HOLD)
ring_colour("#0000FF")
alice.sleep(HOLD)
ring_colour("#FFFFFF")
alice.sleep(HOLD)

-- 2. Ring: each zone separately, so a dead zone is obvious -----------------
for zone = 1, device.led_count do
  local frame = {}
  for i = 1, device.led_count do
    frame[i] = (i == zone) and "#FFFFFF" or "#000000"
  end
  alice.set(frame)
  alice.sleep(700)
end

-- 3. Ring: sweep, to confirm all zones animate smoothly --------------------
for _ = 1, 3 do
  for hot = 1, device.led_count do
    local frame = {}
    for i = 1, device.led_count do
      local level = 255 - math.abs(i - hot) * 110
      if level < 0 then level = 0 end
      frame[i] = string.format("#%02X%02X%02X", level, level, level)
    end
    alice.set(frame)
    alice.sleep(90)
  end
end

-- 4. Clock: every segment at once, then all off ----------------------------
local all = {}
for pos = 0, 3 do
  for _, part in ipairs({"A", "B", "C", "D", "E", "F", "G", "H"}) do
    all[part .. pos] = 200
  end
end
all.DEGREE = 200
all.DH = 200
all.DM = 200
all.DL = 200
alice.clock.segments(all)
alice.sleep(2500)

alice.clock.segments({})
alice.sleep(700)

-- 5. Clock: digit table, to check every glyph ------------------------------
for _, text in ipairs({"0123", "4567", "89--"}) do
  alice.clock.segments(clock_frame(text, 200, 0))
  alice.sleep(1800)
end

-- 6. Clock: colon off, then on. DM must stay dark here ---------------------
local no_colon = clock_frame("1234", 200, 0)
no_colon.DH = 0
no_colon.DL = 0
alice.clock.segments(no_colon)
alice.sleep(HOLD)

alice.clock.segments(clock_frame("1234", 200, 0))
alice.sleep(HOLD)

-- 7. Clock: the middle segment sweeps while 12:34 stays visible ------------
for level = 0, 255, 17 do
  alice.clock.segments(clock_frame("1234", 170, level))
  alice.sleep(70)
end
for level = 255, 0, -17 do
  alice.clock.segments(clock_frame("1234", 170, level))
  alice.sleep(70)
end

-- 8. Clock: degree marker on its own --------------------------------------
local with_degree = clock_frame("1234", 170, 0)
with_degree.DEGREE = 255
alice.clock.segments(with_degree)
alice.sleep(HOLD)

-- 9. Restore ---------------------------------------------------------------
alice.clock.off()
alice.off()
