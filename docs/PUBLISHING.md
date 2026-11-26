# Сборка и публикация пакета

Это руководство для автора пакета. Сборка выполняется на ПК, а установка —
пользователем на колонке. Не копируйте private key или publish token на
колонку и не добавляйте их в Git.

## Что понадобится

- Git и Rust (`cargo`) на ПК;
- исходники YAM;
- namespace и publish token в [YAM Registry](https://repo.yse.wtf/);
- скачанный при создании namespace Ed25519 private key в формате PEM PKCS#8;
- собранные файлы для нужной платформы.

Подготовьте инструменты один раз:

```sh
git clone https://github.com/nyakokitsu/yam.git
cd yam
cargo build --release --locked --bin yam-publish
```

`yam-publish` — PC-инструмент. Он создаёт `.yampkg`, подписывает публикацию и
не устанавливается на колонку.

Перед сборкой полезно прогнать проверку правил публикации: она не пишет архив
и перечисляет всё, что реестр отклонит.

```sh
yam-publish check --source ../my-player
```

Полный список правил и кодов ошибок — в [PUBLISHING-RULES.md](PUBLISHING-RULES.md).
Те же проверки выполняются автоматически в `pack` и `release`; `publish`
загружает уже собранный архив и на них не полагается.

## 1. Создайте namespace и token

1. Откройте [repo.yse.wtf](https://repo.yse.wtf/), войдите через GitHub.
2. В **Профиль → Новый namespace** выберите имя, например `alice`.
3. Нажмите **Создать ключ** и обязательно скачайте private key. Его нельзя
   восстановить; registry получает только public key.
4. После занятия namespace создайте **Publish-токен**. Скопируйте его сразу:
   он показывается один раз.

Сохраните ключ с правами только владельца и передайте token через environment,
а не через аргумент shell history:

```sh
chmod 600 ~/Keys/yam-author-private-key.pem
export YAM_AUTHOR_KEY_FILE="$HOME/Keys/yam-author-private-key.pem"
export YAM_PUBLISH_TOKEN='вставьте-токен-сюда'
```

Либо сохраните оба значения один раз в конфиг пользователя (файл создаётся с
правами `0600`; `--store-key` копирует PEM в каталог конфигурации):

```sh
yam auth login --registry official \
  --token "$YAM_PUBLISH_TOKEN" \
  --key ~/Keys/yam-author-private-key.pem \
  --store-key
yam auth status --registry official
```

Для CI поместите оба значения в секреты CI. Никогда не печатайте token в логе.

## 2. Подготовьте исходники пакета

Минимальная структура:

```text
my-player/
├── yam.yaml
├── target/aarch64-unknown-linux-musl/release/my-player
└── share/alice.player/readme.txt
```

`yam.yaml`:

```yaml
name: alice/player
version: 1.0.0
license: MIT
description: Мой первый пакет
homepage: https://example.org/player
targets: [yandexmini_2]
files:
  bin/my-player: target/$YAM_TRIPLE/release/my-player
  share/alice.player/: share/alice.player/
```

Имя обязано иметь вид `<namespace>/<name>`. Для стороннего namespace каждый
экспортируемый путь в `share/` должен включать `namespace.name`, поэтому в
примере используется `share/alice.player/`.

Для программы на Rust сначала соберите binary под ABI колонки:

```sh
cargo build --release --target aarch64-unknown-linux-musl
```

Выберите один profile ID из [PLATFORMS.md](PLATFORMS.md). На каждый target
нужен отдельный `.yampkg`; нельзя публиковать aarch64 binary как `armv7` или
как `any`.

## 3. Сначала проверьте архив локально

Команды ниже запускайте из корня репозитория: `yam` берёт профили из
`./platforms`, если не задан `--profiles`.

```sh
yam-publish check --source ../my-player
```

Затем соберите и проверьте установку в отдельном корне:

```sh
./target/release/yam-publish pack \
  --profiles platforms \
  --source ../my-player \
  --output ../my-player/dist/player-yandexmini_2.yampkg

./target/release/yam --platform yandexmini_2 --root /tmp/yam-test-player \
  install --yes ../my-player/dist/player-yandexmini_2.yampkg
./target/release/yam --platform yandexmini_2 --root /tmp/yam-test-player doctor
```

`yam-publish pack` создаёт `MANIFEST.yaml` и SHA-256 для каждого файла. Не распаковывайте
и не меняйте полученный `.yampkg`: после изменения hashes больше не совпадут.

## 4. Сборка и публикация одной командой

`release` собирает **каждый** target из `yam.yaml`, подписывает и загружает
артефакты. Указывать output и target вручную не нужно; токен и ключ берутся из
`auth login` или переменных окружения:

```sh
yam release --source ../my-player
```

Тот же сценарий доступен через PC-инструмент `yam-publish release` (флаги
совпадают).

Для `../my-player` с `targets: [yandexmini_2]` появится
`dist/player-1.0.0-yandexmini_2.yampkg`, и он сразу уйдёт в реестр. Если
targets несколько, будет собрано и загружено по артефакту на каждый.

Проверить сборку без загрузки (token и ключ не нужны):

```sh
yam-publish release --source ../my-player --dry-run
```

Параметры:

| Флаг | Назначение |
| --- | --- |
| `--source <каталог>` | Исходники с `yam.yaml`; по умолчанию `.` |
| `--platform <id>` | Собрать только один target вместо всех |
| `--output-dir <каталог>` | Куда писать архивы; по умолчанию `<source>/dist` |
| `-o, --output <файл>` | Точный путь архива; только для одного target |
| `--dry-run` | Собрать и проверить, не загружать |
| `--wait` | Дождаться решения модерации по каждой заявке |
| `--profiles <каталог>` | Профили платформ; по умолчанию `platforms` |

Или используйте готовый shell-пример:

```sh
YAM_PUBLISH_TOKEN='...' \
YAM_AUTHOR_KEY_FILE="$HOME/Keys/yam-author-private-key.pem" \
./tools/build-and-publish-example.sh yandexmini_2 ../my-player \
  ../my-player/dist/player-yandexmini_2.yampkg
```

Нижнеуровневые команды остаются для CI и отладки:

```text
yam-publish check    # проверить правила публикации, ничего не собирая
yam-publish pack     # только создать .yampkg
yam-publish publish  # подписать и отправить существующий .yampkg
yam-publish release  # check + pack всех target + publish
yam-publish status   # состояние заявки, --watch для живого слежения
```

Для другого сервера публикации укажите endpoint явно:

```sh
yam-publish publish ... --upload-url https://registry.example/api/v2/upload
```

Это **не** client URL `/v2`, который пользователи добавляют через `yam registry
add`; upload endpoint выдаёт registry.

## 5. Дождитесь модерации и проверьте публикацию

Загрузка не делает версию видимой сразу: новая версия попадает в очередь
модерации. `publish` печатает `submission id`; следить за ним можно так:

```sh
yam-publish status sub_01JABCDEF --watch
yam-publish status sub_01JABCDEF --watch --json   # по строке JSON на переход
```

`pending → in_review → approved | rejected`; при отказе печатается причина.
Тот же режим доступен сразу при публикации: `release ... --wait`.

После `approved` (или `published`) дождитесь обновления каталога, затем на
чистом YAM root выполните:

```sh
yam search player
yam plan alice/player
yam install alice/player
yam list
```

Контракт endpoint'ов модерации — в [REGISTRY-API.md](REGISTRY-API.md).
Старые реестры без модерации могут ответить текстом `uploaded` без `submission id`; клиент считает
артефакт опубликованным и предупреждает, что следить не за чем.

Опубликованная комбинация имени, версии и target неизменяема. Если нашли ошибку,
увеличьте SemVer-версию, пересоберите и выпустите новый artifact — не
перезаписывайте старый архив.

## Частые ошибки

| Сообщение | Что проверить |
| --- | --- |
| `registry rejected publication` | token активен, namespace принадлежит key, версия ещё не опубликована |
| `author signature does not bind this upload` | используйте тот же private PEM, что был создан для namespace; не меняйте архив после подписи |
| `artifact checksum mismatch` | загрузите новый архив через `yam-publish`, не публикуйте файл вручную |
| пакет не находится | имя указывается как `namespace/name`; дождитесь обновления подписанного index |
| несовместимый target | пересоберите native binary под target профиля и публикуйте отдельный `.yampkg` |
