# Odium Launcher (PortableMC)

Портативный лаунчер Minecraft Java для Windows. По умолчанию данные хранятся в
`%LOCALAPPDATA%\OdiumLauncher`, а `portable.flag` рядом с `.exe` (или запуск с
съёмного диска) переносит их в папку `launcher_data` рядом с лаунчером — без
установки, реестра и прав администратора. Лаунчер сам находит или скачивает
подходящую Java (8/17/21 в зависимости от версии Minecraft), ставит
Fabric/Quilt или Forge/NeoForge, синхронизирует модпак, добавляет сервер в
список и запускает игру.

## Возможности

- Портативные пути: всё рассчитывается от расположения `.exe`, а не от текущей
  директории.
- Источники сборок: JSON-манифест, Modrinth `.mrpack`, встроенный vanilla,
  локальный `file://`.
- Загрузки: потоковая запись в `.part`, ограничение параллелизма, SHA-1/256/512,
  докачка через `Range`, кэш хэшей по size+mtime.
- Безопасная распаковка ZIP: защита от zip-slip и симлинков, лимиты размера.
- Загрузчики: Fabric, Quilt (profile JSON), Forge, NeoForge (headless installer).
- Сборки: дельта-синк, optional files, overrides, автоматическое удаление
  устаревших файлов.
- Сброс сборки: кнопка «Удалить файлы сборки» удаляет моды, конфиги и кэш
  манифеста, но сохраняет миры, скриншоты и логи; следующий запуск скачивает
  пак заново.
- Пять тем: Windows 98 / 7 / 8.1 / 10 / 11; для Win7 — Aero-стекло.
- Публикация: GitHub Releases для `.mrpack`, Git Data API для manifest-паков,
  сборка готового `dist/` + ZIP.
- Диагностика: настройки → «Экспорт диагностики» (один файл для поддержки).
- Кооперативная отмена: загрузки и сетевые этапы останавливаются за секунды.

## Сборка

Требования: Rust 1.82+, Windows 10/11 (для UI), Git.

```powershell
cargo build --release
# target/release/OdiumLauncher.exe
```

### macOS

Сборка выполняется на macOS (Intel или Apple Silicon) с установленными
Rust и Xcode Command Line Tools. Скрипт собирает универсальный бинарник,
упаковывает `.app`, подписывает его ad-hoc и создаёт DMG:

```bash
./tools/package-macos.sh
# dist/OdiumLauncher-macos-universal.dmg
```

Без сертификата Apple Developer приложение не нотаризовано: при первом
запуске на другом Mac его нужно открыть через правый клик → «Открыть»
(либо снять карантин: `xattr -dr com.apple.quarantine "/Applications/Odium Launcher.app"`).
Данные на macOS хранятся в `~/Library/Application Support/OdiumLauncher`.

Тег `v*` запускает GitHub Actions, который собирает Windows `.exe` и
macOS `.dmg` и прикладывает файлы к релизу.

Проверки, которые выполняет CI:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
powershell -ExecutionPolicy Bypass -File tools/Check-RepoHygiene.ps1
```

## Запуск и хранение данных

По умолчанию лаунчер пишет изменяемые файлы в
`%LOCALAPPDATA%\OdiumLauncher` (на других ОС — XDG data dir). Это позволяет
держать `.exe` на рабочем столе и не засорять папку Downloads.

Портативный режим включается одним из способов:

- положить пустой файл `portable.flag` рядом с `OdiumLauncher.exe`;
- задать переменную окружения `ODIUM_PORTABLE=1`;
- запустить лаунчер с USB-накопителя или SD-карты (определяется автоматически).

В этом режиме рядом с `.exe` создаётся:

```text
OdiumLauncher.exe
portable.flag
launcher_data/
  config.json                 настройки игрока и источники
  logs/                       launcher.log-* и game-*.log (хранятся 14 дней)
  meta/                       кэш манифестов и .mrpack
  java/                       скачанные JRE (Temurin 8/17/21)
  libraries/                  библиотеки Minecraft и загрузчиков
  assets/                     ресурсы и natives
  versions/                   version JSON и client.jar
  instances/<source_id>/      файлы конкретной сборки
```

Перенос на другой компьютер — скопировать `.exe`, `portable.flag` и
`launcher_data` целиком. Папка `dist/`, которую собирает «Издатель», уже
содержит `portable.flag`, поэтому у игрока данные останутся внутри сборки.
`config.json` пишется атомарно; повреждённый файл сохраняется как
`config.json.broken.<timestamp>`.

### Пример `config.json`

```json
{
  "schema_version": 1,
  "nick": "Player",
  "sources": [
    {
      "id": "my-pack",
      "name": "My Pack 1.0.0",
      "manifest_url": "https://raw.githubusercontent.com/owner/repo/main/manifest.json"
    }
  ],
  "active_source_id": "my-pack",
  "jvm": { "xms": 1024, "xmx": 6144, "auto_xmx": true },
  "server": { "ip": "127.0.0.1", "port": 25565 },
  "optional_mods_overrides": {
    "my-pack": { "mods/shaders.jar": false }
  },
  "close_launcher_on_start": true,
  "download_concurrency": 8,
  "welcome_seen": true,
  "theme": "win98"
}
```

`kind` у источника можно не указывать: `.mrpack` определяется по расширению,
остальное считается JSON-манифестом. Допустимые схемы URL: `https://`,
`http://`, `file://`, `builtin://`.

## Жизненный цикл игры

- Пайплайн: манифест → Java → загрузчик → синхронизация → Minecraft → JVM.
- После `GameStarted` лаунчер больше не держит процесс в пайплайне: отдельная
  задача ждёт завершения и присылает `GameExited`.
- Если включено «Закрывать лаунчер после запуска», окно закрывается сразу после
  старта JVM. Игра продолжает работать: stdout/stderr пишутся в
  `launcher_data/logs/game-*.log`, а не в pipe.
- Пока лаунчер открыт, журнал игры обновляется каждые 500 мс. Повторный запуск
  заблокирован, пока игра не завершится (или пока окно не закрыто).
- «Отмена» работает на этапах загрузки/подготовки. Уже начатый процесс Java
  отменой не убивается — это осознанное поведение.

## Безопасность

- Все файлы с манифестом проверяются по хэшу; `.part` не считается готовым
  файлом.
- ZIP/mrpack распаковываются с проверкой относительных путей, защитой от
  symlink и лимитами на число записей и суммарный объём.
- Артефакты Minecraft (client jar, libraries, natives, asset index) принимаются
  только по HTTPS с хостов Mojang, Fabric, Quilt, Forge и NeoForge. Сторонний
  loader-профиль не может подсунуть `file://` или свой хост в classpath.
- Источник по `http://` помечается в UI предупреждением: издателю стоит включить
  HTTPS.
- GitHub-токен вводится в память UI и не сохраняется на диск. В legacy-скрипте
  `tools/Publish-Pack.ps1` токен передаётся git через credential helper из
  переменной окружения, а не через URL/аргументы процесса.
- `tools/Check-RepoHygiene.ps1` в CI запрещает коммитить `target/`,
  `launcher_data/`, `dist/`, готовые ZIP/EXE и похожие на секреты строки.

## Брендирование

Домен не нужно править под новый дистрибутив: значения задаются переменными
окружения на этапе сборки.

```powershell
$env:ODIUM_BRAND_NAME          = "My Launcher"
$env:ODIUM_SERVER_LIST_NAME    = "My Server"
$env:ODIUM_DEFAULT_SOURCE_ID   = "my-pack"
$env:ODIUM_DEFAULT_SOURCE_NAME = "My Pack"
$env:ODIUM_DEFAULT_SOURCE_URL  = "https://example.com/manifest.json"
$env:ODIUM_DEFAULT_SERVER_IP   = "127.0.0.1"
$env:ODIUM_DEFAULT_SERVER_PORT = "25565"
cargo build --release
```

Без переменных остаются текущие значения Odium. Готовый `dist/` всегда
содержит собственный `config.json`, поэтому конкретная сборка игрока не зависит
от этих дефолтов.

## Публикация сборки

Канонический путь — окно «Издатель» в лаунчере (крейт `pml-publish`, GitHub REST
API). PowerShell-скрипты в `tools/` оставлены как legacy/dev-обёртки; подробности
и правила синхронизации — в [tools/README.md](tools/README.md).

## Документация

- [docs/manifest.md](docs/manifest.md) — схема манифеста и `.mrpack`.
- [CONTRIBUTING.md](CONTRIBUTING.md) — архитектура, тесты, добавление загрузчика.
- [tools/README.md](tools/README.md) — импорт `.mrpack`, публикация, проверка
  гигиены репозитория.

## Ограничения

- UI и интеграция со стеклом рассчитаны на Windows; на других ОС лаунчер
  собирается, но оконные эффекты и часть путей номинальны.
- Offline-режим использует offline UUID (`MD5` v3); аккаунты Microsoft не
  поддерживаются.
- Forge/NeoForge ставятся внешним installer'ом: диагностика ограничена его
  выводом и таймаутом.
- Публикация через `pml-publish` требует токен с `repo` / `Contents: write`.

## Лицензия

Проект распространяется по лицензии **0BSD (Zero-Clause BSD)** — это
open-source лицензия, одобренная OSI. Она разрешает использовать, копировать,
изменять и распространять лаунчер в исходниках и бинарниках в любых целях,
включая коммерческие, **без указания авторства**. Полный текст — в
[LICENSE](LICENSE).

Сторонние зависимости сохраняют собственные лицензии (в основном MIT и
Apache-2.0); их условия действуют на соответствующие библиотеки. Полный список
крейтов и тексты их лицензий — в
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
