# Lua API

Lua-пакет получает `alice`, `device` и `event`. Укажите минимальный список `permissions` в `plugin.toml`; API без разрешения недоступен.

`device.room` — общая комната колонки (строка либо `nil`), заданная пользователем
в YAM, не в конкретном пакете. Binary runtime `GET /device` и management
`GET /api/device` возвращают то же поле `room` (либо JSON `null`). Для настройки:
интерфейс «Комната», `yam room "Кухня"`, `GET/PUT /api/room`.
[Схема, хранение и совместимость](ROOMS.md). Комната не меняет аппаратную модель
и capabilities. Снимок Lua обновляется при следующем запуске пакета.

| Permission | API |
| --- | --- |
| `led` | `alice.set(colors)`, `off()`, `play(path)`, `logo.set(colors)`, `logo.off()` |
| `volume` | `alice.volume.get()`, `set(percent)` |
| `audio` | `alice.audio.play(path)` |
| `bluetooth` | `alice.bluetooth.status()`, `power()`, `scan()`, `pair()`, `connect()`, `disconnect()`, `remove()` |
| `display` | `alice.display.backlights()`, `set_backlight()` |
| `sensors` | `alice.sensors.list()`, `read(id)` |
| `sensors.control` | `alice.sensors.configure_ambient_light()` |
| `clock` | `alice.clock.available()`, `time()`, `segments()`, `off()` |
| `input` | `alice.input.devices()`, `next_event(timeout_ms)` |
| `network` | `alice.http.get(url)`, `post(url, body)` |
| `ipc.alarm` | `alice.ipc.next_alarm(timeout_ms)` |
| `ipc.speech` | `alice.ipc.next_speech(timeout_ms)` |
| `ipc.read` | `alice.ipc.next(service, timeout_ms)` |
| `ipc.control` | `alice.ipc.send(service, message)`; вместе с `ipc.read` — `request(service, message, timeout_ms)` |
| `commands` | `alice.command.listen(phrase, handler)` |
| `system` | `alice.system.hostname()`, `uptime()`, `meminfo()` |
| `exec` | `alice.exec(program, args)` |

`alice.sleep(ms)` доступен всем пакетам и нужен в циклах для отмены запуска.
Низкоуровневый формат и процессы-владельцы потоков описаны в
[Local Quasar IPC](IPC_PROTOCOL.md).

## Наличие API и модель

`alice.is_available(name)` возвращает, доступна ли возможность на текущей
колонке. Это проверка аппаратной/системной возможности, а не permission:
разрешение всё равно должно быть объявлено, иначе соответствующий API не
появится в `alice`.

Поддерживаются имена `led`, `volume`, `audio`, `bluetooth`, `display`,
`sensors`, `sensors.control`, `clock`, `input`, `network`, `ipc.alarm`,
`ipc.speech`, `ipc.read`, `ipc.control`, `commands` и `system`. Для `exec`
проверка намеренно не предоставляется.

`alice.model()` (и `device.model`) возвращает нормализованный ID модели для
совместимости пакетов, например `yandexmicro`. Варианты строк из device tree,
`--model` и `device.toml` приводятся к этому формату.

```lua
if alice.model() == "yandexmicro" and alice.is_available("led") then
  alice.set({"#00FF00"})
end
```

## Совместимость и сеть

Указывайте нужные возможности, а не модель:

```toml
[compatibility]
features = ["led.ring.rgb"]

[network]
allow = ["api.example.org"]
```

`network` допускает HTTPS только к объявленным публичным хостам. Для HTTP добавьте `allow_insecure_http = true`. Локальные и private-адреса запрещены.

## Input и IPC

Для `alice.input.next_event` перечислите exact kernel device names; прочие `/dev/input` пакет не получает:

```toml
permissions = ["input"]
[input]
devices = ["mute", "vad_keypad"]
```

Функция возвращает `nil` по тайм-ауту либо `{ device, type, code, value, time_ms }`. `alice.ipc.next_alarm` и `next_speech` возвращают `nil` либо событие будильника/распознанной речи. Текст речи — персональные данные: не отправляйте его без явной необходимости и permission `network`.

Для raw Quasar IPC перечислите сервисы в `[ipc].services`. `next` возвращает бинарную строку с protobuf `QuasarMessage`; `send` принимает такую строку. `request` требует `request_id` (field 1) в сообщении и ждёт ответ с тем же ID.

```toml
permissions = ["ipc.read", "ipc.control"]
[ipc]
services = ["mediad"]
```

`ipc.control` может менять состояние системных сервисов. Выдавайте его только доверенным пакетам и только необходимым сервисам.

## Голосовые команды

Сервисный Lua-пакет может обработать распознанную фразу без raw protobuf. Объявите разрешённые фразы в манифесте:

```toml
permissions = ["commands"]

[commands]
phrases = ["ананас"]
```

```lua
alice.command.listen("ананас", function(message)
  -- «алиса ананас» и «ананас» совпадут с этой командой.
  alice.command.cancel() -- отменяет нативный диалог Алисы
  alice.set({"#FFFF00", "#FFFF00", "#FFFF00"})
  -- message.transcript — исходная распознанная фраза.
  -- message.command — нормализованная объявленная команда.
  -- message.source — "voice" или "command".
end)
```

`listen` работает, пока пакет запущен; используйте его в service-пакете. Фразы сравниваются без учёта регистра, повторных пробелов и пунктуации. Обработчик получает только совпавшие фразы.

`alice.command.cancel()` отправляет в `aliced` фиксированную команду `CANCEL_DIALOG`. Вызывайте её только когда пакет действительно перехватил команду: она останавливает текущий нативный диалог Алисы.
