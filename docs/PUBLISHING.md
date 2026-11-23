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

В репозитории YAM есть PC-инструмент `yam-publish`. Он:

1. создаёт `.yampkg` из `yam.yaml`;
2. читает имя и версию из `yam.yaml`;
3. считает SHA-256 архива;
4. подписывает `(name, version, target, sha256)` вашим Ed25519 key;
5. отправляет архив и metadata в registry по HTTPS.

Пример для исходников `../my-player`:

```sh
./target/release/yam-publish release \
  --source ../my-player \
  --profiles platforms \
  --output ../my-player/dist/player-yandexmini_2.yampkg \
  --token "$YAM_PUBLISH_TOKEN" \
  --key "$YAM_AUTHOR_KEY_FILE"
```

Или используйте готовый shell-пример:

```sh
YAM_PUBLISH_TOKEN='...' \
YAM_AUTHOR_KEY_FILE="$HOME/Keys/yam-author-private-key.pem" \
./tools/build-and-publish-example.sh yandexmini_2 ../my-player \
  ../my-player/dist/player-yandexmini_2.yampkg
```

Параметры инструмента:

```text
yam-publish pack     # только создать .yampkg
yam-publish publish  # подписать и отправить существующий .yampkg
yam-publish release  # pack + publish
```

Для другого сервера публикации укажите endpoint явно:

```sh
yam-publish publish ... --upload-url https://registry.example/api/v2/upload
```

Это **не** client URL `/v2`, который пользователи добавляют через `yam registry
add`; upload endpoint выдаёт registry.

## 5. Проверьте публикацию как пользователь

После успешного ответа registry дождитесь обновления каталога, затем на чистом
YAM root выполните:

```sh
yam search player
yam plan alice/player
yam install alice/player
yam list
```

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
