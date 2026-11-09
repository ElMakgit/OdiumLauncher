# Contributing

## Архитектура

Зависимости направлены «вниз»: UI оркестрирует доменные крейты, доменные крейты
не зависят от egui.

| Крейт | Ответственность |
|---|---|
| `pml-config` | портативные пути, `config.json`, валидация, атомарная запись, полное удаление данных |
| `pml-downloader` | HTTP-стриминг, `.part`, хэши, resume, безопасный ZIP |
| `pml-minecraft` | Mojang metadata, библиотеки/ассеты/natives, аргументы, `servers.dat`, `options.txt` |
| `pml-loader` | Fabric/Quilt profile JSON, Forge/NeoForge installer, встроенный офлайн-набор и зеркала |
| `pml-java` | поиск Java, Temurin 8/17/21 через Adoptium |
| `pml-modpack` | manifest, `.mrpack`, дельта-синк, overrides |
| `pml-launcher` | сборка JVM-команды, spawn, лог, tail |
| `pml-publish` | GitHub REST, `dist/` + ZIP |
| `pml-theme` | палитры и виджеты пяти тем |
| `pml-ui` | окно, пайплайн, издатель, welcome, диагностика |

`pml-ui` намеренно разделён:

```text
crates/pml-ui/src/
  lib.rs              только модули и re-export
  app.rs              состояние окна и eframe::App
  app/pipeline.rs     manifest -> Java -> loader -> sync -> Minecraft
  app/publisher.rs    окно издателя и dist
  app/views.rs        панели, настройки, ошибки
  app/welcome.rs      первый запуск
  app/diagnostics.rs  экспорт диагностики
  app/util.rs         RAM, форматирование, открытие папок
```

Правило: в `update`/`draw_*` нет сетевого и файлового I/O. Всё долгое уходит в
Tokio-задачу и возвращается через `mpsc`.

## Как добавить загрузчик

1. Добавьте вариант в `LoaderKind` (`pml-config`) и его `Display`/`label`.
2. Реализуйте установку в `pml-loader`: либо profile JSON (как Fabric/Quilt),
   либо headless installer (как Forge/NeoForge).
3. Верните `LoaderProfile` с путями к version JSON и библиотекам.
4. Добавьте фикстуру с реальным ответом meta/installer и тест на merge.
5. Проверьте `cargo test -p pml-loader` и ручной запуск сборки.

## Как добавить формат источника

1. Расширьте `SourceKind` и `SourceKind::detect`.
2. Реализуйте загрузку/парсинг в `pml-modpack` и верните `LoadedPack`.
3. Валидируйте все относительные пути и хэши до записи на диск.
4. Добавьте тест на повреждённый/враждебный архив.

## Тесты

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Сильные стороны текущего покрытия: downloader (HTTP, resume, hash cache,
zip-slip), config (repair/validation), modpack/mrpack (optional, безопасная
распаковка), minecraft (аргументы, rules, NBT, options), publish (rewrite/dist),
java (парсинг/выбор).

При изменении сетевого кода добавляйте локальный HTTP-сервер на
`tokio::net::TcpListener`, а не реальный запрос к GitHub/Mojang. Для форматов
используйте маленькие JSON-фикстуры в тесте.

### Отмена

`Downloader::with_cancel(Arc<AtomicBool>)` возвращает клон, который проверяет
флаг между чанками. Пайплайн дополнительно оборачивает долгие metadata-запросы
в `tokio::select!` с `wait_for_cancel`. Если добавляете новый долгий этап,
проверяйте флаг до и после него и не удаляйте валидный `.part` при отмене.

## Гигиена репозитория

Не коммитьте `target/`, `launcher_data/`, `dist/`, `*.exe`, `*.pdb`, `*.log`,
`pack/*/manifest.local.json` и готовые ZIP. Перед push:

```powershell
powershell -ExecutionPolicy Bypass -File tools/Check-RepoHygiene.ps1
```

CI запускает ту же проверку. Секреты не должны попадать в URL, аргументы
процессов и логи; для git используйте credential helper с переменной окружения
(см. `tools/Publish-Pack.ps1`).

## Стиль

- `cargo fmt` обязателен.
- Ошибки — `thiserror`, контекст — `tracing`, без `unwrap()` в рантайме.
- Комментарии и идентификаторы — на английском; пользовательские строки UI — на
  русском.
- Публичные API доменных крейтов не должны знать про egui.
- Для новых файлов — атомарная запись или `.part` + rename.
