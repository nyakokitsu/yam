# YAM

Менеджер пакетов для Яндекс Станций.

> [!CAUTION]
> yam обновился: теперь это пакетный менеджер, который работает прямо на
> станции, а Lua-рантайм вынесен в отдельный пакет. Совместимость со старыми
> пакетами потеряна.

## Установка

**Linux / macOS**

```sh
curl -fsSL https://hackalice.me/install.sh | sh
```

**Альтернативная ссылка для Linux / macOS** (если короткий URL ещё обновляется):

```sh
curl -fsSL https://raw.githubusercontent.com/nyakokitsu/yam/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://hackalice.me/install.ps1 | iex
```

**Альтернативная ссылка для Windows PowerShell** (если короткий URL ещё обновляется):

```powershell
irm https://raw.githubusercontent.com/nyakokitsu/yam/main/install.ps1 | iex
```

Установщик подключается к колонке через ADB. Для станций публикуются Linux ARM
binaries `aarch64-unknown-linux-musl` и `armv7-unknown-linux-musleabihf`;
платформа определяется по модели и Device Tree. При необходимости её можно
задать явно:

```sh
curl -fsSL https://hackalice.me/install.sh | sh -s -- --platform yandexmini_2
```

В тех же GitHub Releases лежат десктопные сборки `yam` для ПК:

| Платформа | Ассет |
| --- | --- |
| Linux x86_64 | `yam-x86_64-unknown-linux-musl` |
| Windows x64 | `yam-x86_64-pc-windows-msvc.exe` |
| macOS ARM64 | `yam-aarch64-apple-darwin` |

На ПК `yam` не требует `--platform` для `check`, `pack`, `publish`,
`release`, `status` и `auth`. Станционные команды (`install`, `remove`,
`list`, `update`, …) по-прежнему требуют профиль платформы. `yam-publish`
остаётся совместимым алиасом тех же команд публикации.

Параметры установщика:

| `install.sh` | `install.ps1` | Назначение |
| --- | --- | --- |
| аргумент `[версия]` или `YAM_VERSION` | `-Version` или `YAM_VERSION` | Релиз `yam-v<версия>`. По умолчанию `install.sh` берёт первый тег `yam-v*` из GitHub Releases без фильтра prerelease, `install.ps1` — первый не-draft и не-prerelease |
| `--platform <id>`, `-p <id>` или `YAM_PLATFORM` | `-Platform` или `YAM_PLATFORM` | Профиль платформы вместо автоопределения |
| `YAM_ROOT` | `YAM_ROOT` | Каталог YAM на колонке; по умолчанию `/data/yam`; абсолютный путь из символов `A-Za-z0-9 . _ -` |
| `--remove-old` | `-RemoveOld` | Попытаться остановить legacy `yamd` и **полностью удалить** `/data/quazar/yam` |
| `--uninstall` | `-Uninstall` | Удалить YAM из `YAM_ROOT` (по умолчанию `/data/yam`) вместе с пакетами, базой и данными; также убрать созданные установщиком `/usr/bin/yam` и `/etc/profile.d/yam.sh`. Перед удалением спрашивает подтверждение |
| `--yes`, `-y` | `-Yes` | Не спрашивать подтверждение при удалении |
| — | `-StationIp` | IP колонки для `adb connect` |

Если ADB-устройств несколько, установщик спрашивает номер; если их нет —
IP-адрес и подключается к `<IP>:5555`. Аргументы `install.sh` через пайп:

```sh
curl -fsSL https://hackalice.me/install.sh | sh -s -- 0.2.1
```

Удаление и передача параметров Windows-установщику:

```sh
curl -fsSL https://hackalice.me/install.sh | sh -s -- --uninstall
```

```powershell
& ([scriptblock]::Create((irm https://hackalice.me/install.ps1))) -Uninstall
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/nyakokitsu/yam/refs/heads/main/install.ps1))) -Uninstall -Yes
```

Обёртка `hackalice.me/install.ps1` пропускает только `-Version` и `-Uninstall`;
`-Yes`, `-Platform`, `-StationIp` и `-RemoveOld` передавайте скрипту по прямому
адресу из второго примера.

> [!WARNING]
> `--remove-old` выполняет `rm -rf /data/quazar/yam`: удаляются старый `yamd`,
> все пакеты v1 и их данные. Перед запуском сделайте резервную копию.
>
> `--uninstall`/`-Uninstall` выполняет `rm -rf` каталога `YAM_ROOT` (по
> умолчанию `/data/yam`): удаляются все пакеты v2, база состояния и их данные.
> Entware (`/opt`) и каталог блокировки `/tmp/yam` не затрагиваются.

## Использование

```sh
yam registry add official https://repo.yse.wtf/v2 \
  --key ed25519:5Rz8F47uftWVU5kLgWYhprkEZ4rpP2kecUge/a7KQas=

yam registry list
yam install yam/luna
```

Реестр `official` по умолчанию не настроен: перед `search` и установкой по
имени добавьте его командой `yam registry add` (см. [CLI](docs/CLI.md)). После
установки команда `yam` доступна на станции напрямую: установщик создаёт
`/usr/bin/yam` как ссылку на `$YAM_ROOT/bin/yam-run` и добавляет `$YAM_ROOT/bin`
в `PATH` через `/etc/profile.d/yam.sh`. Обёртка задаёт платформу, каталог
профилей и корень `YAM_ROOT`, поэтому отдельные флаги для обычных команд не
нужны.

Для локальной проверки пакета укажите отдельный каталог. Профили при этом
берутся из `./platforms` (запускайте из корня репозитория) или из
`--profiles`:

```sh
yam --platform yandexmicro --root ./yam-root install --yes package.yampkg
```

## Публикация пакета с ПК

Сначала один раз сохраните токен и ключ namespace в конфиг пользователя:

```sh
yam auth login --registry official \
  --token "$YAM_PUBLISH_TOKEN" \
  --key "$HOME/Keys/yam-author-private-key.pem" \
  --store-key
```

`auth status` показывает реестр, отпечаток токена и наличие ключа, не печатая
сам токен. Дальше публикация — одна команда: собираются все target из
`yam.yaml`, архивы подписываются и уходят в реестр, yamreg публикует их сразу. `--wait` следит за модерацией только в реестрах с очередью.

```sh
yam release --source ../my-player --wait
yam status <submission-id> --watch  # только для реестра с очередью
```

`--dry-run` собирает и проверяет пакеты без загрузки. Подробности —
[PUBLISHING.md](docs/PUBLISHING.md), правила — [PUBLISHING-RULES.md](docs/PUBLISHING-RULES.md),
контракт реестра — [REGISTRY-API.md](docs/REGISTRY-API.md).

## Документация

- [Каталог документации](docs/README.md)
- [Командная строка](docs/CLI.md)
- [Упаковка](docs/PACKAGING.md)
- [Правила публикации](docs/PUBLISHING-RULES.md)
- [Реестр](docs/REGISTRY.md)
- [API реестра](docs/REGISTRY-API.md)
- [Платформы](docs/PLATFORMS.md)
- [Быстрый старт](docs/QUICKSTART.md)
- [Сборка и публикация](docs/PUBLISHING.md)

Lua, Quasar, веб-интерфейс и старые `.yap`/`plugin.toml` находятся в отдельном
проекте [Luna](https://github.com/nyakokitsu/luna), а не в ядре YAM.

## Обновление

На всех поддерживаемых платформах установщик создаёт `/usr/bin/yam` и файл
`/etc/profile.d/yam.sh`. На `yandexmicro` дополнительно создаётся загрузочный
скрипт `/etc/init.d/S99yam`, который при старте выполняет `yam update` (ошибка
обновления загрузку не прерывает). На остальных платформах автообновления при
загрузке нет.

Обновление принимается только при корректной Ed25519-подписи манифеста из
GitHub Releases; открытый ключ зашивается в бинарник при сборке. Для
публикации нужны repository variable `YAM_UPDATE_PUBLIC_KEY` и secret
`YAM_UPDATE_SIGNING_KEY`.
