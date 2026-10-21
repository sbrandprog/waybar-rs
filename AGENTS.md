# AGENTS.md

Руководство для ИИ-агентов и разработчиков в этом репозитории.

## 1. Что это

`waybar-rs` — самостоятельный статус-бар для Wayland на Rust поверх **GTK4 +
gtk4-layer-shell**. Вырос из переписывания [Waybar](https://github.com/Alexays/Waybar)
(C++ / GTK3 / gtkmm3), но развивается независимо. Формат и параметры конфигурации
могут сильно различаться.

В корне есть необязательный симлинк `waybar-original -> ../Waybar`
(в `.gitignore`) — только историческая справка, обращаться к нему не требуется.

## 2. Раскладка

```
Cargo.toml                   # workspace (resolver 3, edition 2024)
app/                         # бинарник waybar-rs: CLI, runtime, владение барами
crates/core/                 # инфраструктура бара (см. §3)
crates/modules/              # модули (feature-gated) + label.rs
crates/modules/sysinfo/      # чистые парсеры /proc и /sys
examples/                    # config.jsonc + style.css для разработки
resources/icons/             # символические иконки (privacy и т.п.)
vendor/system-tray/          # system-tray 0.8.9, вендорен и пропатчен (tray, §10)
waybar-original -> ../Waybar # симлинк на оригинал (необязателен)
```

Тулчейн: Rust 1.98+, edition 2024 (let-chains).

## 3. `waybar-core`

| Модуль | Ответственность |
|---|---|
| `config.rs` | `Config::load/parse` (JSONC через `json5`), `BarConfig`, `Position`, `OutputFilter` (`!`, `$ENV`, `*`, массивы), margins |
| `bar.rs` | layer-shell окно, якоря/маргины/layer/exclusive, `CenterBox` + `modules-left/center/right`, создание модулей из `Registry`; `monitors()` — GDK-мониторы |
| `module.rs` | `Module` trait, `ModuleInit`, `Registry`, `split_name` (`battery#bat2`) |
| `label.rs` (modules) | `LabelModule` — базовый label-модуль: states, icons, format-цепочки, interval, видимость |
| `format.rs` | свой format-движок (`{name:spec}`, `{{}}`); числовые аргументы — `FmtArg::Int`/`Float` |
| `timer.rs` | `every_seconds`, `schedule_aligned` (выравнивание по границе + `TimerHandle`) и `debounce` |
| `bridge.rs` | `channel()` + `on_main(rx, handler)` — доставка из фоновых задач в GTK-поток |
| `udev.rs` | libudev-хотплаг: `watch(subsystems)` → поток на подписку (RAII `Subscription`), uevent через `async_channel` |
| `power.rs` | logind `PrepareForSleep`: `on_resume(cb)` + RAII `ResumeHandle` |
| `widgets.rs` | `attach_actions` (on-click*/on-scroll*), `Tooltip`, `tooltip_enabled` |
| `action.rs` | `spawn_command` — запуск shell-команд |
| `css.rs` | `Css`: загрузка `style.css`, hot-reload с рекурсивным обходом `@import` |
| `icons.rs` | добавление `resources/icons/` в поиск иконок GTK |

## 4. Контракт модуля

```rust
pub struct ModuleInit {
    pub name: String,    // "battery#bat2" — полное имя из конфига
    pub kind: String,    // "battery" — база для фабрики
    pub id: String,      // суффикс после '#'
    pub config: Value,   // объект опций модуля (может быть Null)
    pub bar: BarConfig,
    pub window: gtk::ApplicationWindow,  // для оконных классов (battery-<state>)
    pub section: String, // modules-left|center|right
}

pub trait Module: 'static {
    fn widget(&self) -> gtk::Widget;  // вставляется в контейнер секции
    fn start(&self) {}                // таймеры/подписки; вызывается после вставки
}
```

Регистрация — `register(reg: &mut Registry)` в файле модуля, вызов из
`waybar_modules::register` под `#[cfg(feature = ...)]`.

## 5. Как добавить модуль

1. Создать `crates/modules/src/<name>.rs`.
2. При необходимости — чистый парсер в `crates/modules/src/sysinfo/<name>.rs`
   (функции принимают текст, а не путь).
3. `mod <name>;` и вызов `register` в `crates/modules/src/lib.rs` под `cfg(feature)`.
4. Фича в `crates/modules/Cargo.toml` (по умолчанию включена, если нужна).
5. Юнит-тесты на чистую логику; smoke-проверка на `examples/config.jsonc`.

## 6. Соглашения

**GTK/потоки.** GTK-виджеты не `Send`. Из фоновых задач и async — только через
`bridge::on_main`; мутировать виджеты строго в главном потоке. Дорогие/блокирующие
операции — не в главном потоке.

**Формат-движок.** Числовые плейсхолдеры (`{usage:03}`) обязаны приходить как
`FmtArg::Int`, а не `Text` (иначе width/zero-pad не применяется). Плейсхолдеры без
`Args` заменяются пустой строкой; `format_ctx` один раз на `(модуль, плейсхолдер,
шаблон)` пишет `warn!`. Легитимные плейсхолдеры обрабатывать до/после форматирования
(пример — `{timeTo}` в `battery`).

**CSS и дерево виджетов.** Наш контракт: `window#waybar` (через
`set_widget_name`), `modules-left/center/right`, `#<module>`, `#sway-workspace-<name>`,
класс `.module`, служебные классы состояний (`warning`, `critical`, `charging`,
`locked`, ...). Менять — осознанно (ломает написанный `style.css`).
Техзаметка: в `gtk4 0.11` метод — `set_widget_name`.

**Пространство имён попапов.** В GTK CSS нет `@scope`, поэтому попап несёт свой
style-class (схема `<module>-menu`), все правила — его потомки; это изолирует меню
от глобальных селекторов `style.css`. Так сделаны `tray` (`.tray-menu modelbutton:hover`)
и `clock`-календарь (`.clock-menu .clock-day.selected`).

**Обёртка модуля.** Виджет модуля — безымянный `Box`, внутри — именованный
контейнер (`#<module>`/label). Обёртка нужна, чтобы `.modules-* > * > *` матчили
модуль целиком, а не каждый дочерний элемент (иначе margin складывается со
`spacing`). Модули, чей корень — именованный контейнер, обязаны её добавлять;
видимость переключается на обёртке.

**Независимость от апстрима.** При неочевидной семантике (порядок format-цепочек,
метрики, тултипы) решаем сами и фиксируем здесь, а не сверяемся с оригиналом.

**Парсеры.** Логика разбора — в чистых функциях, принимающих текст; тесты на
фикстурах. Никаких `unwrap()` на данных из внешних файлов — `Option`/`unwrap_or`.

**Ошибки и логи.** `anyhow::Result` на границах, `tracing::{warn,error,info,debug}`
внутри. Паника в GTK-колбэке убивает бар — вместо неё `warn!` и деградация.

**Стиль.** `cargo fmt`; `cargo clippy --all-targets` без предупреждений.

**Комментарии.** Осознанные решения и «почему» фиксируются здесь, а не в коде;
в коде — только короткий `//!`-заголовок со ссылкой на этот документ.

**Feature-флаги.** Один модуль — одна фича; отключённая фича не тянет зависимость.

## 7. Сборка, запуск, проверки

```sh
cargo build                      # отладочная сборка
cargo test                       # юнит-тесты
cargo clippy --all-targets       # без предупреждений

# dev-бар (exclusive=false, --non-unique — рядом с основным)
cargo run -p waybar-rs -- --non-unique -c examples/config.jsonc -s examples/style.css

# подробные логи
WAYBAR_RS_LOG=debug cargo run -p waybar-rs -- --non-unique -c examples/config.jsonc -s examples/style.css
```

По умолчанию читаются `$XDG_CONFIG_HOME/waybar/config.jsonc` и `style.css`.
**Smoke-тест:** запустить на несколько секунд (`timeout -s TERM 4 ...`) и убедиться,
что нет ошибок создания модулей. `Gdk-WARNING` про `portal.Inhibit` игнорируем.

## 8. Git

Conventional commits (`feat(modules): ...`, `fix: ...`, `docs: ...`). Один логический
блок — один коммит; сообщение объясняет «почему». Перед коммитом: `cargo test` +
`cargo clippy --all-targets` чисто. Исключение — бамп версии
(`chore: bump workspace version to X.Y.Z`, строго одна строка).

## 9. Состояние

Этапы порта 1–6 закрыты (история): каркас; информеры (`clock`, `cpu`, `memory`,
`battery`, `backlight`); `sway/*` + `keyboard-state` + `custom`; D-Bus
(`idle_inhibitor`, `power-profiles-daemon`, `network`); аудио/видео + tray
(`wireplumber`, `privacy`, `tray`); полировка (CSS под GTK4, hot-reload,
мультимонитор). **Дальнейшее — самостоятельное**, критерий паритета не действует.

## 10. Решения и ограничения

- **Один инстанс + dev-бар.** Дефолтный `application_id`
  (`io.github.sbrandprog.waybar_rs`) — single-instance: повторный запуск
  активирует существующие бары. `--non-unique` (`NON_UNIQUE`) не занимает имя на
  шине и строит свои бары — для разработки рядом с основным. Оговорки:
  `exec_always "pkill waybar-rs; …"` при reload убьёт и dev-бар; dev с основным
  конфигом даёт два tray-хоста (для этого `examples/config.jsonc`). `application_id`
  — D-Bus well-known name: ≥ 2 элемента через `.`, `[A-Za-z0-9_-]`, не с цифры,
  reverse-DNS, `-` → `_`.
- **Мультимониторные бары.** На каждый монитор — по бару на каждый конфиг,
  подходящий под `output` (без `output` → на всех). Hotplug — через `items-changed`
  у `Display::monitors()` (в `gdk4` нет add/remove), удаление отложено в idle.
  `output-dimensions` не поддержан.
- **CSS под GTK4.** `style.css` читается как есть; селекторы GTK3
  `widget:first-child`/`widget:last-child` в GTK4 молча не матчатся (в живом конфиге
  заменены на `*:first-child`/`*:last-child`). Ошибки парсинга логируются
  (`parsing-error`).
- **Hot-reload CSS** — только `reload_style_on_change`: мониторятся `style.css` и
  все `@import`; полного reload конфига по `SIGUSR2` и `on-sigusr1/2` нет.
- **Курсор интерактивных модулей.** В GTK4 нет CSS `cursor`, он ставится программно
  (`set_click_cursor`/`set_scroll_cursor`) на корень модуля **и всё поддерево**:
  при нажатии GTK вешает implicit grab на глубокий виджет, и курсор только на корне
  мерцал бы. Есть клик → `pointer`; иначе скролл → курсор по осям; клик приоритетнее.
  `custom` с одним `exec-on-event` курсора не получает. Встроенный скролл: `backlight`
  — вертикаль, `sway/workspaces` — обе оси + `pointer` на кнопках, `tray` — `pointer`
  по иконкам. Обход `apply_cursor` **не спускается в `GtkPopover`** (календарь, меню);
  интерактивные элементы попапа ставят курсор сами.
- **Тултипы — штатный `GtkTooltip` с принудительным пере-показом.** GTK прячет
  тултип на каждом `GDK_BUTTON_PRESS`/`GDK_SCROLL` и показывает лишь по
  `trigger_tooltip_query` при следующем движении указателя, поэтому клик без сдвига
  курсора оставлял тултип скрытым. `widgets::Tooltip` держит штатный `GtkTooltip`, но
  запоминает активный `gtk::Tooltip` и факт наведения (`EventControllerMotion`), а по
  нажатию/скроллу в idle зовёт `trigger_tooltip_query()` — только если тултип уже был
  показан. `set` при активном тултипе обновляет markup на месте (без мерцания).
- **Пробуждение после suspend.** GLib-таймеры идут по `CLOCK_MONOTONIC`, который в
  suspend стоит, поэтому тик опаздывает до интервала (для `clock` с 60 с заметно).
  `core::power` подписывается на logind `PrepareForSleep` (через `gtk::gio`, без новых
  зависимостей; RAII `ResumeHandle` снимается при `Drop`) и по пробуждению зовёт
  колбэки; `clock` перерисовывается и **пере-армирует** `schedule_aligned` новым
  `TimerHandle` (одной перерисовки мало — старая фаза неверна). Без logind — `warn!`
  и работа по таймерам. Остальные таймерные модули не подписаны (интервалы малы).
- **Backlight пишет только через logind.** Читается из sysfs (`/sys/class/backlight`),
  пишется методом `Session.SetBrightness` по D-Bus (zbus) — udev-rule и права на
  `/sys` не нужны. Соединение ленивое. Шаг — `max(1, round(step*max/100))`,
  база — `brightness` (запрошенное), не `actual_brightness`.
- **Tokio-runtime подключён.** `core::bridge::spawn` — глобальный multi-thread runtime,
  `on_main` доставляет в GTK-поток. Через него: `sway/*`, `custom/*`, `keyboard-state`,
  `tray`, PipeWire-модули (свой поток).
- **`sway/language`** линкует `libxkbregistry` (FFI в `sway/xkb.rs`) ради `{short}`.
- **`sway/workspaces`** не поддерживает `window-rewrite`/`enable-bar-scroll`.
  Переупорядочивание кнопок ручное (в GTK4 у `GtkBox` нет абсолютного
  `reorder_child`). `ignore-workspaces` — регистронезависимый `^(?:…)$`.
  Persistent-воркспейс переключается без `move workspace to output`. `event_worker`
  не переживает модуль: `Drop` шлёт стоп-сигнал, воркер соревнует `events.next()` и
  `stop.recv()` (через `poll_fn`, без tokio), затем роняет соединения и sender —
  иначе хотплаг копил бы воркеры и IPC.
- **`keyboard-state`** держит `/dev/input/event*` с тремя lock-LED. Весь IO — в одном
  потоке (`waybar-keyboard`), владеющем `evdev::Device`: `poll(2)` + обязательный
  `Device::fetch_events` (непрочитанная очередь evdev навсегда оставляет `POLLIN` и
  крутит 100% CPU). Источник истины — тик `LedCommand::Resync` (1 с) через
  `EVIOCGLED`; в GUI уходит только изменение. `interval`: число/строка — секунды,
  `0`/`"once"` — событийный режим. Таймер гасится с модулем (`SourceId::remove`),
  поток — по `LedCommand::Stop` при `Drop`. Хотплаг — по uevent `input` (открыть/
  удалить один `eventN`, без рескана). Переоткрывать все устройства каждый тик нельзя
  (`evdev::Device::open` ест ~90–100 мс и морозит бар). `device-path` фильтрует.
  Иконки/форматы — один раз при старте; индекс LED едет вместе с ключом.
- **Format-движок** не поддерживает hex/oct/bin (`x/o/b`) и флаг `#` — `warn!` и
  десятичное представление.
- **`clock` — свой парсер strftime** (`{:%H:%M}`), без fmt-индексов. `format`/
  `format-alt`/`tooltip-format` — Pango-разметка (литеральные `&`/`<` экранирует
  пользователь); разметка парсится один раз (`pango::parse_markup`), спецификаторы —
  один раз (`StrftimeItems::parse_to_owned`). Неизвестная таймзона/локаль — фатальная
  ошибка конфигурации (модуль показывает короткий текст и не тикает). Битая разметка/
  спецификатор — `warn!` (с дедупом повторов) и текст ошибки, без падения. `interval`
  по умолчанию выводится из label-форматов (`format`/`format-alt`): есть секунды —
  1 с, иначе 60 с; явный `interval` главнее, `"once"` — один рендер. Тултип
  рендерится по запросу (`query-tooltip`), поэтому не влияет на `interval`; без
  `tooltip-format` берёт `format`, если тот валиден. Левый клик переключает
  `format`/`format-alt` только без команды левого клика (`on-click*`); label в
  обработчике держится слабой ссылкой (иначе цикл-утечка при hotplug). Тики
  выравниваются по границе (`timer::schedule_aligned`, пол 10 мс). `locale` —
  `chrono::Locale` (POSIX-строка нормализуется); `timezone` — IANA через `chrono-tz`
  (`""`/`local` → системная); `{:%Z}` для именованной зоны — аббревиатура, для
  локальной — числовой offset. `timezones`/`{timezone}`/`timezone-tooltip-format` не
  реализованы; календарь — отдельно.
- **`clock`-зависимости.** Модуль по умолчанию собирается с `chrono`
  (`features = ["clock", "unstable-locales"]` → `pure-rust-locales`) и `chrono-tz`
  (встроенная tzdata): `locale`/`timezone` — часть контракта, на под-фичи не делятся.
  Цена — время сборки и размер бинарника (для бара приемлемо).
- **`clock`-календарь** — опциональный (`calendar`: `true`/объект) попап `GtkPopover`,
  нарисованный вручную на `chrono` (не `GtkCalendar`), поэтому подчиняется
  `locale`/`timezone` модуля (у объекта свой `timezone`-override; неизвестный — warn
  и наследование; пустая строка — тоже наследование). Правое меню его открывает,
  левый клик по-прежнему переключает format; сконфигурированный `on-click-right`
  вырезается с warn, а `on-double/triple-click-right` заставляют встроенный одиночный
  ждать. Клик по дню — `.selected`, «сегодня» — `.today`. Опции: `week-start`
  (`monday`/`sunday`), `week-numbers`, `show-weekdays`, `other-months`. Вид только
  месячный (`mode: year` не реализован). CSS-неймспейс `.clock-menu` +
  `.clock-calendar`/`.clock-nav`/`.clock-title`/`.clock-weekday`/`.clock-week-number`/
  `.clock-day` (+ `.today`, `.selected`, `.other-month`). Внутри намеренно нет
  expanding-виджетов (иначе попап растягивает модуль и сдвигает соседей). Заголовок —
  `CenterBox`, равные колонки — `Grid::set_column_homogeneous`. `pointer` ставится
  точечно на `‹`/`›` и ячейки дней.
- **`custom`** хайдит виджет при пустом выводе или коде возврата ≠ 0. `escape`
  экранирует и текст, и тултип: при `return-type: raw` первая строка даёт оба, вторая
  (`tooltip`) экранируется всегда. `interval`/`restart-interval` принимают числовые
  строки наравне с числами (`"interval": "5"` — интервальный режим). Скрипт стартует
  в своей process group (`process_group(0)` + `PR_SET_PDEATHSIG`); по `Drop` группа
  получает `SIGTERM`, через секунду — `SIGKILL`. `image-path`/`image-name` не
  реализованы.
- **`idle_inhibitor`** — через Wayland idle-inhibit (своё соединение и инхибитор на
  приватном `wl_surface`), не через logind.
- **`network` через `nmrs`** (NetworkManager D-Bus), не netlink/nl80211: `{essid}`/
  `{ipaddr}`/`{cidr}`/`{ifname}`/`{icon}` — из NM; `{rxBitrate}`/`{txBitrate}` —
  максимальная скорость активной AP; `{gwaddr}` — `/proc/net/route`; bandwidth —
  `/proc/net/dev`; linkSpeed — `Device.speed_mbps`; `{icon}` — `Strength`.
- **`wireplumber` — нативный `pipewire`** (metadata `default.audio.sink` + Props/Route),
  без libwireplumber и `wpctl`. `{volume}` — кубический корень линейного усиления
  (берётся пик каналов; каналы масштабируются одним коэффициентом, поэтому баланс
  сохраняется). Для hardware-синков mute и громкость авторитетны из device `Route`,
  а не node `Props` (иначе mute/громкость путались бы); пока route не прочитан, снапшот
  с громкостью не эмитится и модуль скрыт, а не показывает 100%. Если device пропал
  или не биндится, route-индекс сбрасывается и громкость снова берётся из node `Props`,
  чтобы модуль не остался скрытым навсегда. Запись идёт только в route, когда он
  известен; `mute` в route пишется только из самого route — node-овый mute туда не
  подставляется (иначе скролл размутил бы железо). Частичные апдейты мержатся;
  разреженные info-события узла не теряют `node.nick`/`description`/`name` и
  route-индекс того же устройства. `{node_name}` экранируется перед markup.
  `{volume_db}` при нуле — `-60`. Скролл строит на последнем записанном значении
  (окно 500 мс), чтобы серия событий колеса давала все шаги. `{icon}` ключуется по
  `node.name` (+ `-muted`, `default-muted`), а не по display-name, как в апстриме.
  Не реализованы `only-physical`, вторичный source (`{source_*}`/`format-source`),
  form-factor ключи иконок, reconnect.
- **`privacy` — нативный `pipewire`**: классификация по `media.class`
  (`Stream/Input/Video`, `Stream/Input/Audio`, `Stream/Output/Audio`), активность —
  состояние `RUNNING`. Тултип — кастомный GTK4-виджет (`gtk::Tooltip::set_custom`);
  он пересобирается только при изменении набора узлов. Свой поток останавливается
  по `Drop` модуля (`Cmd::Stop`), иначе hotplug монитора копил бы потоки и виджеты.
  Иконка приложения — первое совпадение в теме среди `application.icon-name` →
  `application.process.binary` → portal app id → `application.name` → `node.name`
  (с повторной попыткой в нижнем регистре: pulse-потоки дают «Firefox» без
  `icon-name`, реальное имя — в `process.binary`). Props мержатся по присутствующим
  ключам. GeoClue2 `location` не реализован: в дефолтные индикаторы не входит, при
  явной настройке пропускается с `warn!`.
- **`tray` — вендоренный `system-tray 0.8.9`** (`vendor/system-tray/`, патчи в
  `PATCHES.md`) + свой GTK4-рендер; меню — `gtk::PopoverMenu` из `gio::Menu`.
  Крейт не отдаёт SNI `Scroll`/`ContextMenu`, поэтому скролл по иконкам и
  fallback-меню не работают; `orders`, `reverse-direction`, per-app `icons`,
  overlay-иконки не реализованы. Вендорен ради тихого завершения: финальные сигналы
  гонятся с исчезновением D-Bus-имени и читают свойства у мёртвого имени (шум
  `WARN`/`ERROR`, menu-watcher не останавливался); патч останавливает watcher, чинит
  панику debounce и опускает ожидаемые ошибки до `debug`. Виджет — безымянная обёртка
  вокруг `#tray`-бокса (`§6`), поэтому margin не вешается на каждую иконку. Порядок
  иконок фиксирован кодом (`order_key` = app id / имя шины, `reorder_child_after`).
  `subscribe()` вызывается до `items()`. Тултип: `<b>title</b>\nDescription` при
  непустом описании, иначе просто `title` (всё экранируется); пустой апдейт не
  сбрасывает показанное. Меню — `modelbutton` (`modelbutton.flat:hover`), CSS-неймспейс
  `.tray-menu`; на каждый лист свой `gio::SimpleAction` (`enabled` → `set_enabled`,
  `Checkmark` → boolean-state, `Radio` → string-state), истина — обновление `DBusMenu`.
- **Иконки privacy** лежат в `resources/icons/*.svg` и добавляются в icon theme через
  `core::icons::install`; читаются рядом с бинарником или из workspace.

## 11. Полезные крейты

| Крейт | Для чего |
|---|---|
| `gtk4 0.11` (features `v4_12`) | UI |
| `gtk4-layer-shell 0.8` | layer-shell |
| `json5` → `serde_json` | JSONC-конфиг |
| `chrono` (`clock`, `unstable-locales`) / `chrono-tz 0.10` | clock, локали, таймзоны |
| `swayipc-async 3.0` | sway-модули |
| `evdev 0.13` | keyboard-state |
| `tokio 1` | runtime, `process` для custom |
| `zbus 5` | `power-profiles-daemon`, backlight |
| `nmrs 3.5` | network через NetworkManager |
| `wayland-client 0.31` + `wayland-protocols 0.32` | idle_inhibitor |
| `system-tray 0.8.9` (вендорен) | tray |
| `pipewire 0.10` | wireplumber, privacy |
| `udev 0.9` | хотплаг battery/backlight (`libudev-dev`/`systemd-devel`) |
| `libc 0.2` | `poll(2)` в udev-мониторе и потоке `keyboard-state` |

**Важно про `gtk4`:** пару `gtk4`/`gtk4-layer-shell` подбирать согласованно — Cargo
запрещает два пакета с `links = "gtk-4"`; проверенные версии — в `Cargo.toml`.
