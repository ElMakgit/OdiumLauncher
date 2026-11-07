# Формат манифеста и `.mrpack`

## JSON-манифест

Манифест — единственный источник истины для версии Minecraft, загрузчика,
сервера и управляемых файлов инстанса. Схема версионируется полем
`schema_version` (сейчас `1`).

```json
{
  "schema_version": 1,
  "name": "My Pack",
  "version": "1.2.0",
  "mc_version": "1.21.1",
  "loader": { "type": "fabric", "version": "0.16.5" },
  "server": { "ip": "play.example.com", "port": 25565 },
  "files": [
    {
      "path": "mods/example.jar",
      "url": "https://cdn.example.com/example.jar",
      "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "size": 123456
    }
  ],
  "optional_files": [
    {
      "path": "mods/shaders.jar",
      "url": "https://cdn.example.com/shaders.jar",
      "sha1": "0123456789abcdef0123456789abcdef01234567",
      "size": 7890
    }
  ]
}
```

### Поля

| Поле | Обязательно | Смысл |
|---|---|---|
| `schema_version` | нет (по умолчанию 1) | версия схемы, `0` и `>1` отклоняются |
| `name` | да | отображаемое имя сборки |
| `version` | нет | версия сборки для UI/публикации |
| `mc_version` | да | версия Minecraft, например `1.21.1` |
| `loader.type` | да | `fabric`, `quilt`, `forge`, `neoforge` |
| `loader.version` | да | версия загрузчика |
| `server` | нет | добавляется в `servers.dat`; иначе берётся `config.server` |
| `files` | нет | обязательные файлы |
| `optional_files` | нет | файлы, которые игрок может отключить |

### `files` / `optional_files`

- `path` — относительный путь внутри `launcher_data/instances/<source_id>/`.
  Абсолютные пути, `..`, двоеточия и пустые сегменты запрещены.
- `url` — `https://`, `http://` или `file://`.
- Хэш: достаточно одного из `sha256`, `sha512`, `sha1`. Приоритет —
  SHA-256 → SHA-512 → SHA-1. Длина и hex-формат проверяются.
- `size` — необязательный ожидаемый размер; используется для прогресса и
  проверки результата.
- Дубликаты путей (с учётом нормализации `\` → `/`) отклоняются.

### Optional files

Файл из `optional_files` включён, пока в `config.json` нет явного `false`:

```json
"optional_mods_overrides": {
  "my-pack": {
    "mods/shaders.jar": false
  }
}
```

UI показывает список дополнительных компонентов в панели сборки. Изменение
применяется при следующем запуске: синхронизация удалит отключённый файл и
скачает включённый.

### Состояние синхронизации

После успешного батча в инстансе пишется
`.pml_manifest_state.json` (`path -> hash`). Следующий запуск пропускает файлы с
тем же хэшем и удаляет записи, которых больше нет в манифесте. Состояние
обновляется только после успешной проверки всех хэшей.

## Modrinth `.mrpack`

`.mrpack` — ZIP с `modrinth.index.json`:

```json
{
  "formatVersion": 1,
  "game": "minecraft",
  "versionId": "1.2.0",
  "name": "My Pack",
  "files": [
    {
      "path": "mods/example.jar",
      "hashes": { "sha1": "...", "sha512": "..." },
      "downloads": ["https://cdn.modrinth.com/data/.../example.jar"],
      "fileSize": 123456
    }
  ],
  "dependencies": {
    "minecraft": "1.21.1",
    "fabric-loader": "0.16.5"
  }
}
```

Лаунчер:

1. Скачивает `.mrpack` с conditional-запросами (`ETag`/`Last-Modified`) и
   кэширует его в `meta/mrpack/`.
2. Преобразует `files` в `Manifest.files`; `downloads[0]` становится `url`,
   хэши переносятся как `sha1`/`sha512`.
3. Определяет загрузчик по `dependencies`.
4. После дельта-синка распаковывает `overrides/` и `client-overrides/` в инстанс
   с теми же защитами от zip-slip/симлинков и лимитами.

Поддерживается `formatVersion = 1` и `game = "minecraft"`.

## Публикация manifest-пака

Каноническая реализация — `pml-publish` (окно «Издатель»). Она:

1. Читает `manifest.json` и все файлы `overrides/**`.
2. Переписывает URL каждого override на
   `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/overrides/<path>`
   (сегменты URL кодируются).
3. Добавляет в манифест файлы, которых в нём ещё нет, с SHA-256 и размером.
4. Загружает дерево одним коммитом через Git Data API.
5. Опционально собирает `dist/`: `OdiumLauncher.exe`, `launcher_data/config.json`
   с URL манифеста, `README.txt` и ZIP.

`tools/Publish-Pack.ps1` делает то же самое через git и оставлен как
legacy/dev-обёртка. При изменении правил rewrite/hash/URL сначала меняется
`pml-publish`, затем скрипт; расхождение считается багом.

## Проверки, которые нельзя обходить

- `schema_version` и обязательные поля.
- Относительные безопасные пути.
- Наличие хотя бы одного валидного хэша.
- Дубликаты путей.
- Размер манифеста (лимит в `pml-modpack`) и лимиты overrides в `.mrpack`.
- HTTPS не обязателен технически, но `http://` помечается в UI как риск
  подмены.
