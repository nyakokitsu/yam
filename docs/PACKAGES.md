# Пакеты

`.yap` — ZIP или `tar.gz` с `plugin.toml` в корне. Пути внутри пакета только относительные.

```toml
id = "ambient-lights"
name = "Ambient lights"
version = "1.0.0"
entry = "main.lua"
permissions = ["led"]

[launch]
kind = "lua"       # lua | binary
path = "main.lua"

[compatibility]
models = ["yandexplum"] # необязательно: поддерживаемые модели
features = ["led.ring.rgb"]
```

Перед использованием включите пакет. Любой binary-пакет можно запустить разово через `yam run` (stdout/stderr выводится в терминал) или в фоне через `yam service start`. `yam service enable` включает автозапуск после перезапуска daemon. Поле `launch.mode` старых архивов принимается для совместимости, но больше не ограничивает способ запуска.

Binary-пакет требует `exec`, должен соответствовать архитектуре колонки (обычно AArch64 musl) и может задать `args`, `interpreter = "sh"` и `library_paths`. Его вывод доступен через `yam logs <id>`.

Binary-пакет запускается в собственной группе процессов. При остановке вся группа получает SIGTERM, а через 2 секунды — SIGKILL, поэтому дочерние процессы (`aplay`, `arecord`, конвейеры shell) завершаются вместе с пакетом. Процессы, которые сами ушли в другую сессию или группу (`setsid`), YAM не отслеживает.

```sh
yam install package.yap          # локальный архив
yam install nano                 # последняя версия из registry
yam install nano 8.0             # конкретная версия
yam enable zip                   # включить пакет для ручного запуска
yam run zip --help                # аргументы передаются без обязательного --
yam tui nano                     # интерактивный binary через ADB
yam service enable sensor-agent  # включить автозапуск и запустить в фоне
yam service logs sensor-agent
yam service restart sensor-agent
yam service stop sensor-agent
yam rm nano
```

Binary runtime получает `YAM_API_URL` и одноразовый `YAM_API_TOKEN`; передавайте токен в `X-YAM-Token`. Доступны `/device`, `/sensors`, `/led`, `/led/off`, `/clock`, `/volume`, `/system` и `/bluetooth` согласно permissions пакета. Для Bluetooth actions используются `/bluetooth/{power,scan,pair,connect,disconnect,remove}`. `/logo` и `/logo/off` доступны только если текущая аппаратная реализация их поддерживает.
