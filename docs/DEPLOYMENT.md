# Установка

Нужен `adb`. Установщик находит колонку, проверяет SHA-256 релизных файлов и ставит `yamd` в `/data/quazar/yam`.
Он также создаёт `/etc/init.d/S99yam`, поэтому `yamd` и включённые service-пакеты запускаются после перезагрузки колонки.

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/yam/main/install.sh | sh
```

Windows:

```powershell
irm https://raw.githubusercontent.com/nyakokitsu/yam/main/install.ps1 | iex
```

Если колонка не видна в `adb devices`, передайте адрес без интерактивного
вопроса — это особенно удобно при запуске через `irm | iex`:

```powershell
$env:YAM_STATION_IP = "192.168.1.42"
irm https://raw.githubusercontent.com/nyakokitsu/yam/main/install.ps1 | iex
```

Адрес с суффиксом `:5555` также принимается.

Для конкретной версии передайте её первым аргументом (`sh -s -- 0.1.13`). Удаление на Unix: `sh -s -- --uninstall`.

CLI обращается к `http://<колонка>:8099`; адрес можно переопределить `YAM_URL`.

## Безопасность и обновления

Web UI не имеет аутентификации: используйте только доверенную LAN. Автообновление включено; отключение в `settings.toml`:

```toml
[updates]
enabled = false
```
