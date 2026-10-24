-- Самотест для Яндекс Станции Лайт 2 (plum).
--
-- Показывает по шагам кольцо и все 36 сегментов экрана, задерживая каждое
-- состояние настолько, чтобы его можно было проверить глазами.
--
-- Особенности платформы:
--   * кольцо — 3 зоны по RGB: plum:{red,green,blue}:ring1..3
--   * экран — 36 сегментов: A..H на позициях 0..3, плюс DEGREE, DH, DM, DL
--   * alice.clock.segments() перезаписывает кадр целиком: всё, что не
--     перечислено, гасится. Поэтому каждый кадр собирается полностью.
--   * DH и DL — двоеточие; DM — отдельный средний сегмент для полоски,
--     в двоеточие не входит.
--   * в песочнице доступны только table/string/math/utf8, поэтому print()
--     не используется.

local HOLD = 1500 -- мс на один видимый шаг

-- Шрифт цифр, как в штатной прошивке.
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

-- ring_colour заливает все зоны одним цветом.
local function ring_colour(hex)
  local frame = {}
  for i = 1, device.led_count do
    frame[i] = hex
  end
  alice.set(frame)
end

-- clock_frame рисует четыре символа, двоеточие и уровень полоски.
-- Позиции нумеруются с 0, как в sysfs.
local function clock_frame(text, brightness, bar, colon)
  local frame = {}
  for pos = 0, 3 do
    local char = string.sub(text, pos + 1, pos + 1)
    local parts = FONT[char] or FONT[" "]
    for _, part in ipairs(parts) do
      frame[part .. pos] = brightness
    end
  end
  local mark = 0
  if colon then mark = brightness end
  frame.DH = mark
  frame.DL = mark
  frame.DM = bar or 0
  return frame
end

-- 1. Кольцо: основные цвета ------------------------------------------------
ring_colour("#FF0000")
alice.sleep(HOLD)
ring_colour("#00FF00")
alice.sleep(HOLD)
ring_colour("#0000FF")
alice.sleep(HOLD)
ring_colour("#FFFFFF")
alice.sleep(HOLD)

-- 2. Кольцо: каждая зона отдельно — сразу видно мёртвую зону ----------------
for zone = 1, device.led_count do
  local frame = {}
  for i = 1, device.led_count do
    frame[i] = (i == zone) and "#FFFFFF" or "#000000"
  end
  alice.set(frame)
  alice.sleep(700)
end

-- 3. Кольцо: бегущий огонь --------------------------------------------------
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

-- 4. Экран: все сегменты разом, потом гашение ------------------------------
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

-- 5. Экран: каждый глиф во всех четырёх позициях.
-- Двоеточие выключено, чтобы "4567" не выглядело как битое время.
for digit = 0, 9 do
  local text = string.rep(tostring(digit), 4)
  alice.clock.segments(clock_frame(text, 200, 0, false))
  alice.sleep(900)
end

-- 6. Экран: где именно двоеточие.
-- "1111" без двоеточия и с ним: разница видна только между второй и третьей
-- цифрой, если DH/DL стоят на своём месте.
alice.clock.segments(clock_frame("1111", 200, 0, false))
alice.sleep(1800)
alice.clock.segments(clock_frame("1111", 200, 0, true))
alice.sleep(1800)

-- 7. Экран: нормальное время -----------------------------------------------
alice.clock.segments(clock_frame("1234", 200, 0, true))
alice.sleep(HOLD)

-- 8. Экран: полоска на DM при видимом времени ------------------------------
-- DM меняется, DH/DL остаются ровно на месте.
for level = 0, 255, 17 do
  alice.clock.segments(clock_frame("1234", 170, level, true))
  alice.sleep(70)
end
for level = 255, 0, -17 do
  alice.clock.segments(clock_frame("1234", 170, level, true))
  alice.sleep(70)
end

-- 9. Экран: маркер градуса отдельно ----------------------------------------
local degree = clock_frame("1234", 170, 0, true)
degree.DEGREE = 255
alice.clock.segments(degree)
alice.sleep(HOLD)

-- 10. Возврат ---------------------------------------------------------------
alice.clock.off()
alice.off()
