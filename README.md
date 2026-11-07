# Odium Launcher (PortableMC)

Портативный лаунчер Minecraft Java для Windows. Один `.exe` и папка
`launcher_data` рядом с ним — без установки, реестра и прав администратора.
Лаунчер сам находит или скачивает Java 21 (Temurin), ставит Fabric/Quilt или
Forge/NeoForge, синхронизирует модпак, добавляет сервер в список и запускает игру.

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

Проверки, которые выполняет CI:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
powershell -ExecutionPolicy Bypass -File tools/Check-RepoHygiene.ps1
```

## Запуск и структура `launcher_data`

Запустите `OdiumLauncher.exe`. Рядом появится:

```text
OdiumLauncher.exe
launcher_data/
  config.json                 настройки игрока и источники
  logs/                       launcher.log-* и game-*.log
  meta/                       кэш манифестов и .mrpack
  java/                       скачанные JRE
  libraries/                  библиотеки Minecraft и загрузчиков
  assets/                     ресурсы и natives
  versions/                   version JSON и client.jar
  instances/<source_id>/      файлы конкретной сборки
```

Перенос на другой компьютер — скопировать `.exe` и `launcher_data` целиком.
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
- ZIP/mrpack распаковываются с проверкой относительных путей и лимитов.
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

MIT.
