# Аппаратные возможности платформ Yandex

YAM не линкуется с закрытым SDK колонок. Вместо этого он использует только проверяемые в момент запуска Linux-интерфейсы, поэтому пакет переносим между моделями и не обращается к несуществующим model-specific путям.

## Поддерживаемое обнаружение

| Возможность YAM | Источник в platform SDK | Проверка YAM | Feature |
| --- | --- | --- | --- |
| RGB-кольцо Mini 2 / Micro | `yandexmini_2/.../yandex_mini2_led_controller.cpp`, `yandexmicro/.../yandex_micro_led_controller.cpp` | sysfs LED brightness | `led.ring.rgb` |
| RGB-кольцо Midi | `cucumber:{red,green,blue}:ring0..23` | sysfs LED brightness | `led.ring.rgb` |
| RGB-кольцо Mini 1 | `/system/vendor/quasar/led_util` | нативный проигрыватель `.led` | `led.ring.rgb` |
| Системные паттерны Mini 1 | `/system/vendor/quasar/ledpatterns/*.led` | `alice.play_system` | `led.pattern.system` |
| Сегментные часы | `yandexmini_2/.../yandex_mini2_clock_led_controller.cpp` | все 32 `*clock{A..G}{1..4}/brightness` или 45 `*white:clock-*` (Midi) в одном LED-профиле | `display.clock.segment.v1` |
| Освещённость | `libs/brightness_control/light_sensor.cpp` | `iio:device*/in_intensity_input` | `sensor.ambient_light.iio` |
| Температура | portable PowerManager и стандартный Linux hwmon | thermal-zone/hwmon sysfs | `sensor.temperature.sysfs` |
| Батарея и заряд | `jbl_link/portable/power_manager/power_manager.cpp` | power_supply capacity/status sysfs | `battery.sysfs` |
| Подсветка экрана | Android/TV platform конфигурации | `/sys/class/backlight` | `display.backlight` |
| Кнопки | platform-specific ButtonsHandler | `/sys/class/input` и `/proc/bus/input/devices` | `input.evdev` |

## Mini 1 (yandexmini)

Mini 1 (`YandexStationMini_1C52`, Amlogic AXG) не публикует RGB-кольцо в `/sys/class/leds`: каталог существует, но пуст. Шестью сегментами управляет `/system/vendor/quasar/led_util`, который принимает путь к `.led` и работает до получения сигнала. YAM определяет такую колонку по hostname и наличию `led_util` с каталогом `ledpatterns`, а не по sysfs.

Профиль `yandexmini` передаёт файл паттерна нативному проигрывателю, поэтому поддерживаются директивы `rotate`, `gradient` и `background`, которых нет в sysfs-парсере. `alice.play` берёт `.led` из пакета, `alice.play_system("note_1.led")` — штатный паттерн прошивки. Замена паттерна завершает предыдущий дочерний процесс, а `off()` запускает чёрный кадр и оставляет проигрыватель в известном выключенном состоянии. Пустой `/sys/class/leds` не должен включать capability `led.sysfs`.

## Не предоставляется намеренно

- OTA, factory reset, ADB activation, power-off и прошивка Bluetooth/Zigbee контроллеров;
- platform keys, device cryptography и идентификаторы устройства;
- внутренние IPC SDK: голосовой pipeline, авторизация, облачные директивы и состояние медиа;
- Zigbee NCP: SDK использует специфичные UART-протоколы и persistent network state, которые нельзя безопасно открывать произвольному пакету без отдельного адаптера и явной модели разрешений.

Это ограничение предотвращает блокировку колонки, утечку ключей и повреждение пользовательской конфигурации. При появлении стабильного локального интерфейса для конкретной возможности его следует добавлять как новый capability, профиль оборудования и минимальный permission, а не как raw sysfs/exec доступ.

## SKU и часы

YAM не привязывает сегментный renderer к строке модели: он находит полный набор из 32 sysfs-сегментов и использует общий префикс драйвера. Поэтому один адаптер подходит Станции Мини 2 с часами (`YNDX-00020`), Лайт 2 с часами (`YNDX-00026`), Мини 3 (`YNDX-00027`) и Мини 3 Про (`YNDX-00059`), если их прошивка публикует сегменты через LED sysfs.

У Midi (`YNDX-00054`) своя раскладка: 45 сегментов `cucumber:white:clock-*` (`{a..g}{1..4}`, `p1..5`, `q1..5`, `h/l/m{1,2}`, `deg`) на двух чипах, у каждого защёлка `cucumber:sync:clock-N`: `1` замораживает экран, `0` — живой режим. Framebuffer `fb0` (кольцо) и `fb1` (лого) YAM не трогает.

`YNDX-00060` (Station 3), `YNDX-0008` (Max) и `YNDX-00055` (Duo Max) используют матричный/frontal framebuffer или Android-экран, а не этот 32-сегментный интерфейс. В изученной версии SDK нет общедоступного и одинакового device node/формата кадра для этих SKU. YAM по-прежнему даёт им управление подсветкой через `display.backlight`, но не пишет произвольные байты во framebuffer: это может повредить активный интерфейс. Для точного clock-renderer на этих моделях нужны дампы `/sys/class/graphics/*/name`, `fb_var_screeninfo` и путь к framebuffer с конкретного устройства.
