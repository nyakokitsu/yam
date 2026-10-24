# Разработка

## Требования

- Rust stable и Cargo.
- Docker для воспроизводимых кросс-сборок Linux.
- `zip` для упаковки примеров.
- Android Debug Bridge для развёртывания на колонке через ADB.

## Локальная разработка

```sh
cargo fmt
cargo check
cargo run --bin yamd -- --listen 127.0.0.1:8099 --data-dir ./data
```

Runtime хранит настройки и пакеты в каталоге из `--data-dir`. Удаляйте только временный каталог разработки, если требуется чистое состояние.

## Архитектура

`App` хранит общие настройки, профиль устройства, LED-контроллер и менеджер пакетов. Axum обслуживает Web UI и JSON API. Каждый Lua-плагин выполняется в блокирующей задаче с ограниченным набором стандартных библиотек Lua. Runtime выдаёт таблицы API только при наличии соответствующего permission в манифесте.

Запуск плагинов асинхронный. У работающего плагина есть атомарный токен отмены. `POST /api/plugins/:id/stop` устанавливает токен и выключает LED. Lua-код должен регулярно вызывать `alice.sleep`.

## Профили устройств

При старте YAM проверяет device-tree, файлы ОС, sysfs и наличие команд. Профиль доступен через `GET /api/device`.

Поддерживаемые LED-профили:

| Профиль | Путь к LED | Количество |
| --- | --- | --- |
| `mini2` | `mini2:{red,green,blue}:ringN` | 6 |
| `micro` | `micro:{red,green,blue}:ringN` | 3 |
| `midi` | `cucumber:{red,green,blue}:ringN`, N с нуля | 24 |
| `plum` | `plum:{red,green,blue}:ringN` | 3 |
| `sysfs-ring` | `{red,green,blue}:ringN` | задаётся в настройках |
| `custom` | `path_template` | задаётся в настройках |

Для `custom` шаблон поддерживает `{root}`, `{channel}` и `{index}`. Если настроенное RGB-кольцо доступно, YAM добавляет в профиль `led.ring.rgb`, `led.ring.<count>` и `led.pattern.v1`.

На Станции Миди кольцом штатно управляет platformd через `/dev/fb0` и LED-триггер `ledfb-ring`. Запись в `brightness` снимает триггер, поэтому `off()` в профиле `midi` возвращает `ledfb-ring` на каждый LED.

На Станции Лайт 2 (`plum`) запись в `brightness` триггер `led_ring_fb` не снимает, поэтому `off()` просто гасит каналы.

## Целевые сборки

Station Lite, обнаруженная при разработке, использует AArch64 Linux. Воспроизводимая сборка:

```sh
docker run --rm -v "$PWD":/home/rust/src -w /home/rust/src messense/rust-musl-cross:aarch64-musl cargo build --release
```

Результат: `target/aarch64-unknown-linux-musl/release/yamd`.

Для armhf используйте образ или toolchain для armv7 musl и собирайте бинарник `armv7-unknown-linux-musleabihf`.

## Проверочный список

1. Запустите форматирование и `cargo check`.
2. Запустите runtime с временным data-dir.
3. Установите пример `.yap` пакета.
4. Убедитесь через `GET /api/plugins`, что пакет совместим.
5. Запустите пакет и проверьте `running: true`.
6. Остановите пакет и убедитесь, что железо перешло в безопасное состояние.
