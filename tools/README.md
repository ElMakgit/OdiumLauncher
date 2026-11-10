# tools

Скрипты для подготовки и публикации сборок. PowerShell-часть работает на Windows
и использует `git` из `PATH`; упаковка macOS запускается на macOS.

| Скрипт | Назначение | Статус |
|---|---|---|
| `Import-Mrpack.ps1` | распаковать `.mrpack` в `manifest.json` + `overrides/` | dev |
| `New-OdiumManifest.ps1` | собрать манифест по папке сборки | dev |
| `Publish-Pack.ps1` | переписать URL, запушить в GitHub, собрать `dist/` + ZIP | legacy-обёртка |
| `Check-RepoHygiene.ps1` | запретить артефакты/секреты в git | CI |
| `capture_window.ps1` | снимок окна для проверки UI | dev |
| `Measure-Memory.ps1` | private bytes и working set релизной сборки | release |
| `fetch-offline-bundles.ps1` | скачать и сверить архивы NeoForge для встраивания в exe | release |
| `package-macos.sh` | universal `.app` + `.dmg` (macOS) | CI/release |
| `generate-third-party-notices.py` | собрать `THIRD-PARTY-NOTICES.md` | release |

## Источник истины

Публикация в продукте выполняется крейтом `pml-publish` (окно «Издатель»):
GitHub REST API, Git Data API, тот же rewrite/hash/URL-encoding и `dist/`.
Для `.mrpack` в `dist/launcher_data/config.json` пишется страница репозитория
(`kind: github`), а не точная ссылка на релизный ассет: игрок получает новые
версии автоматически. PowerShell-скрипты повторяют этот конвейер для CI и
старых сценариев. Если меняются правила манифеста, сначала обновляется
Rust-реализация и её тесты, затем `Publish-Pack.ps1`.

## Import-Mrpack.ps1

```powershell
.\Import-Mrpack.ps1 -Mrpack .\pack.mrpack -OutDir .\pack\my-pack
```

Результат: `manifest.json`, `overrides/`, при необходимости `manifest.local.json`.
Файлы из Modrinth сохраняют CDN-URL; в overrides попадают только локальные
конфиги/моды.

## Publish-Pack.ps1

```powershell
$env:GITHUB_TOKEN = '<token>'
.\Publish-Pack.ps1 -PackDir .\pack\my-pack -Repo owner/repo -Tag v1.2.0
# проверка без сети и без записи:
.\Publish-Pack.ps1 -PackDir .\pack\my-pack -Repo owner/repo -DryRun
```

Что делает:

1. Проверяет `manifest.json` и `overrides/`.
2. Переписывает URL overrides на `raw.githubusercontent.com`.
3. Добавляет отсутствующие override-файлы с SHA-256/размером.
4. Собирает `dist/` (`OdiumLauncher.exe`, `launcher_data/config.json`,
   `README.txt`) и ZIP.
5. Клонирует репозиторий, копирует `manifest.json`/`overrides/`, пишет
   `version.json`, коммитит и пушит.

### Токен

- Токен не печатается и не попадает в `git remote -v`, URL клона или аргументы
  процесса.
- Git получает его через `credential.helper`, который читает
  `ODIUM_GIT_TOKEN` из окружения; переменная очищается в `finally`.
- Системные credential helper'ы отключены для этих команд
  (`credential.helper=`), чтобы не использовать чужой кэш.
- При ошибке клона/пуша текст маскируется (`***`) перед выводом.

Проверка вручную:

```powershell
$env:ODIUM_GIT_TOKEN = 'ghp_example'
$helper = '!f() { case "$1" in get) echo username=x-access-token; echo "password=$ODIUM_GIT_TOKEN";; esac; }; f'
"protocol=https`nhost=github.com`n`n" |
  git -c credential.helper= -c "credential.helper=$helper" credential fill
Remove-Item Env:\ODIUM_GIT_TOKEN
```

Ожидаемый вывод: `username=x-access-token` и `password=ghp_example`.

## generate-third-party-notices.py

```bash
python tools/generate-third-party-notices.py
```

Собирает `THIRD-PARTY-NOTICES.md`: список всех сторонних крейтов, их лицензии и
тексты. Для крейтов без лицензионного файла текст берётся из upstream-репозитория.
Файл прикладывается к релизу и копируется внутрь macOS `.app`. Запускать после
обновления `Cargo.lock`.

## Check-RepoHygiene.ps1

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\Check-RepoHygiene.ps1
```

Падает, если в индексе есть `target/`, `launcher_data/`, `dist/`, `pack/`,
`*.exe`, `*.dll`, `*.pdb`, `*.log`, сгенерированные ZIP/`manifest.local.json`,
файл больше 20 MiB или строка, похожая на GitHub-токен/приватный ключ.
Запускается в CI до сборки.

## Measure-Memory.ps1

```powershell
cargo build --release
.\Measure-Memory.ps1 -Label idle -Seconds 30
.\Measure-Memory.ps1 -Scenarios -IdleSeconds 30 -PeakSeconds 15
```

Первый запуск даёт одну цифру (min/median/p95/max для private bytes и working
set), `-Scenarios` прогоняет весь набор: простой, окно на весь экран, возврат
к обычному размеру и 20 циклов свернуть/развернуть. Скрипт сам ждёт появления
окна и прогрева первого кадра; `-KeepRunning` оставляет процесс открытым,
`-ProcessId` подключается к уже запущенному. Результаты 0.1.6 и бюджет — в
[docs/memory.md](../docs/memory.md).

## capture_window.ps1

Вспомогательный снимок окна для проверки темы/вёрстки: находит самое большое
видимое окно процесса (у winit есть служебные окна 16×16) и сохраняет PNG. Не
входит в продуктовый пайплайн.

## fetch-offline-bundles.ps1

```powershell
.\fetch-offline-bundles.ps1            # в dist/neoforge-mirror
.\fetch-offline-bundles.ps1 -Force     # перекачать
```

Скачивает архивы NeoForge из релиза `neoforge-mirror` (и через прокси GitHub,
если прямой адрес недоступен), сверяет размер и SHA-256 с `OFFLINE_BUNDLES` и
кладёт в `dist/neoforge-mirror/`, откуда `crates/pml-loader/build.rs` вшивает их
в exe. Папка в git не попадает — в индексе оказался бы файл больше 20 MiB.
Список архивов и хеши дублируются в скрипте: при обновлении набора правятся оба
места.
