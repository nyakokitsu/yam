# Разработка плагинов

Режимы выполнения, бинарные пакеты и CLI описаны в [документации пакетов](PACKAGES.md).

## Структура пакета

`.yap` — gzip-сжатый TAR-архив. В его корне должен находиться `plugin.toml`.

```text
my-plugin.yap
├── plugin.toml
├── main.lua
├── patterns/
│   └── pulse.led
├── sounds/
│   └── hello.wav
└── bin/
    └── helper
```

```toml
id = "my-plugin"
name = "My plugin"
version = "1.0.0"
entry = "main.lua"
description = "Example package"
permissions = ["led"]

[compatibility]
models = ["yandexmicro"]
features = ["led.ring.rgb", "led.ring.3"]
```

Создание архива:

```sh
cd my-plugin
tar -C package-source -czf my-package.yap plugin.toml main.lua assets/
```

## Lua-окружение

В Lua доступны `table`, `string`, `math` и `utf8`. Библиотеки `os`, `io`, `package` и debug недоступны. Сеть доступна только через permission-gated `alice.http`.

- `event` содержит `manual` при запуске через Web UI.
- `device.model`, `device.architecture`, `device.led_count` и `device.features` описывают колонку.
- `alice` содержит только API, разрешённые permissions пакета.

## Permissions и API

| Permission | API | Требование |
| --- | --- | --- |
| `led` | `alice.set(colors)`, `alice.off()`, `alice.play(path)`, `alice.play_system(name)`, `alice.sleep(ms)` | RGB LED-профиль; для `play_system` — `led.pattern.system` |
| `volume` | `alice.volume.set(percent)`, `alice.volume.get()` | `audio.volume.alsa` |
| `audio` | `alice.audio.play(relative_path)` | `audio.play.aplay` |
| `bluetooth` | `alice.bluetooth.status()`, `power(bool)`, `scan(bool)`, `pair(mac)`, `connect(mac)`, `disconnect(mac)`, `remove(mac)` | `bluetooth.bluez` |
| `display` | `alice.display.backlights()`, `set_backlight(name, value)` | `display.backlight` |
| `sensors` | `alice.sensors.list()`, `read(id)` | `sensor.ambient_light.iio`, `sensor.temperature.sysfs` или `battery.sysfs` |
| `sensors.control` | `alice.sensors.configure_ambient_light(device, gain, integration_ms)` | `sensor.ambient_light.iio` |
| `clock` | `alice.clock.available()`, `time("HH:MM", brightness)`, `segments(levels)`, `off()` | `display.clock.segment.v1` |
| `input` | `alice.input.devices()` | `input.evdev` |
| `network` | `alice.http.get(url)`, `alice.http.post(url, body)` | непустой `[network].allow` |
| `ipc.alarm` | `alice.ipc.next_alarm(timeout_ms)` | `YAM_ALARMD_ADDR` у daemon |
| `ipc.speech` | `alice.ipc.next_speech(timeout_ms)` | `YAM_ALICED_ADDR` у daemon |
| `system` | `alice.system.hostname()`, `uptime()`, `meminfo()` | нет |
| `exec` | `alice.exec(program, args)` | runtime-настройка `allow_plugin_binaries = true` |

`alice.set` принимает ровно `device.led_count` строк формата `#RRGGBB`. `alice.sleep` принимает 0–60000 мс и является точкой отмены анимации.

`alice.play(path)` читает `.led` из каталога пакета. На sysfs-профилях YAM сам проигрывает кадры и функция возвращается после завершения; на нативном профиле `yandexmini` файл передаётся `led_util`, запуск возвращается сразу, а следующая LED-операция заменяет паттерн. `alice.play_system(name)` доступна только при feature `led.pattern.system` и принимает имя файла из `/system/vendor/quasar/ledpatterns`, без пути и `..`.

### HTTP(S)

`network` не открывает raw socket. Пакет обязан перечислить разрешённые хосты в манифесте; допустимы точный хост, суффикс `*.example.org` и явный wildcard `*` для любого публичного хоста. Loopback, private/link-local IP literals и `localhost` отклоняются даже при `*`. По умолчанию разрешён только HTTPS, редиректы отключены, а request/response ограничены 1 МиБ.

```toml
permissions = ["network"]

[network]
allow = ["api.example.org", "*.example.net"]
```

`alice.http.get(url)` и `alice.http.post(url, body)` возвращают `{ status = number, body = string }`. HTTP разрешается только при явном `allow_insecure_http = true` в `[network]`.

### События aliced и alarmd

YAM подключается к **двум разрешённым** IPC-сервисам и декодирует только документированные поля `QuasarMessage`: `AliceState.recognized_phrase` и `AlarmEvent.alarm_fired`. Он не даёт Lua raw protobuf, команды IPC или доступ к другим сервисам.

Администратор daemon задаёт адреса сервисов (обычно loopback-порты из конфигурации прошивки): `YAM_ALARMD_ADDR=127.0.0.1:PORT` и `YAM_ALICED_ADDR=127.0.0.1:PORT`. Это намеренно не настраивается пакетом.

`alice.ipc.next_alarm(timeout_ms)` возвращает `nil` по тайм-ауту либо `{ type = "alarm_fired", id, alarm_type, start_timestamp_ms, duration_seconds, remind_message, currently_playing }`. `alice.ipc.next_speech(timeout_ms)` соответственно возвращает `nil` либо `{ type = "speech", transcript }`. Тайм-аут — до 60 секунд. Распознанная речь относится к персональным данным: запрашивайте `ipc.speech` только при необходимости, предупреждайте пользователя и не передавайте текст в сеть без одновременно объявленного `network`.

### Датчики

`alice.sensors.list()` возвращает массив таблиц `{ id, kind, unit }`, а `alice.sensors.read(id)` — таблицу с дополнительным числовым полем `value`. Поддерживаются обнаруженные в sysfs датчики освещённости IIO, температурные thermal/hwmon и аккумуляторы power_supply. Температура нормализуется в градусы Цельсия, даже когда драйвер возвращает миллиградусы.

Управление gain и временем интеграции опционально и требует отдельный permission `sensors.control`; допустимы gain `1, 3, 6, 9, 18` и `25, 50, 100, 200, 400` мс. Например:

```lua
for _, sensor in ipairs(alice.sensors.list()) do
  if sensor.kind == "ambient_light" then
    local reading = alice.sensors.read(sensor.id)
    alice.sensors.configure_ambient_light(sensor.id:match("^ambient%-light:(.+)$"), 18, 400)
  end
end
```

### Сегментные часы

На колонках с физическим 32-сегментным clock-дисплеем YAM обнаруживает `display.clock.segment.v1`, не привязываясь к имени LED-драйвера. `alice.clock.time("07:35", 170)` записывает четыре цифры и разделитель; яркость — `0..255`, по умолчанию `170`. `segments` принимает таблицу с уровнями сегментов, например `{ A1 = 255, B1 = 255, PLUS1 = 64 }`; доступные имена — `A..G` с позициями `1..4`, `PLUS1`, `PLUS2`, `MINUS`, `DEGREE`. На Миди нет `MINUS`, `PLUS1`/`PLUS2` — точки двоеточия, а дополнительно доступны `H1`, `H2`, `L1`, `L2`, `M1`, `M2`, `P1`, `P2`, `P4`, `P5`, `Q1`, `Q2`, `Q4`, `Q5`. `off()` возвращает кадр platformd, который был на экране до первой записи пакета.

Часы управляются системным platformd одновременно с пакетами, поэтому package должен регулярно перерисовывать свой кадр, если ему нужно удерживать его на экране.

Daemon также предоставляет `GET /api/sensors`, `GET /api/clock` и `PUT /api/clock`. CLI-эквиваленты: `yam sensors`, `yam clock status`, `yam clock set 07:35 --brightness 170`, `yam clock off`.

## Совместимость

`models` проверяется точным совпадением. Для `features` должны присутствовать все перечисленные возможности. При несовместимости YAM сохраняет пакет, но блокирует запуск и показывает причину в Web UI.

По возможности указывайте требования к возможностям, а не к модели. Например, RGB-анимация должна требовать `led.ring.rgb`; точный размер кольца нужен только если от него зависит раскладка кадров.

## Жизненный цикл

Действие запуска асинхронно. Повторный запуск отменяет предыдущее выполнение этого же плагина перед новым стартом. Остановка отправляет отмену и выключает LED-контроллер runtime. В каждом кадре анимации вызывайте `alice.sleep` или `alice.set`.
