## TL;DR

**Уберите `write-file`, `replace-in-file` и обязательные `read_ticket` из публичного API. Оставьте `read-file` и добавьте один `apply_patch`, внутри которого сервер сам читает актуальные снимки файлов, применяет patch к памяти, а перед commit повторно проверяет SHA-256.**

Главное архитектурное изменение: **MCP-handler не должен вызывать другой MCP-handler и разбирать его JSON**. Нужны отдельные типизированные компоненты: parser → planner → remote snapshot store → transaction committer. Ticket сейчас смешивает workflow-проверку «сначала прочитай» и optimistic locking, причём обе задачи решает ненадёжно.

## Дополнение 2026-07-22: права и sudo

Status: implemented; focused unit and Docker validation completed on 2026-07-22.

Текущий `apply_patch` всегда выполняет snapshot и commit от имени обычного SSH-пользователя через `exec_raw_streaming`. Он не использует `sudo`, не переходит на интерактивный `su`-канал и не наследует возможности отдельного `sudo_shell`. Поэтому readable, но недоступный для атомарной замены файл заканчивается ошибкой staging, lock или finalize.

Это должна быть явная privilege boundary:

* обычный `apply_patch` никогда не повышает права автоматически и не повторяет операцию через sudo;
* отказ из-за недоступного parent/staging/finalize должен возвращать стабильную actionable-ошибку, объясняющую, что `apply_patch` не повышает права;
* агент не должен молча переходить на `sudo_shell`: ручной `sed`/`cat` обходит parser, snapshot SHA, lock и atomic commit;
* простой вызов `wrap_sudo_command(apply_cmd, password)` недопустим: patch-контент уже передаётся через stdin, а password-based `sudo -S` использует тот же stdin для пароля и может поглотить payload.

План разделяется на две границы scope:

1. Сначала сделать непривилегированный путь правдивым: распознавать недоступный для записи parent, не терять sanitized stderr при stage/lock/finalize, возвращать `permission_denied` там, где отсутствие доступа подтверждено, и гарантировать отсутствие частичной мутации.
2. Для привилегированного edit добавлен явно разрешаемый `sudo_apply_patch`, а не fallback внутри `apply_patch`. Отдельное имя позволяет harness независимо разрешать privileged tool и соответствует паре `shell`/`sudo_shell`.
3. `sudo_apply_patch` переиспользует тот же parser/planner/CAS/lock/atomic-commit flow, выполняет snapshot и commit в одной privilege-модели, а patch payload передаёт через отдельный private remote stage, не конфликтующий с `sudo -S`.

Минимальные regressions для этой границы:

* Update/Add/Delete в root-owned parent через обычный `apply_patch` дают понятный permission error и не меняют target;
* наличие настроенного sudo не вызывает неявного повышения прав;
* `sudo_apply_patch` отдельно проходит passwordless и password-based sudo, conflict detection и проверку неизменности target при ошибке.

---

# Что сейчас происходит в проекте

Сейчас зависимость примерно такая:

```text
read-file
  └── читает файл
      ├── формирует публичный MCP JSON
      └── выпускает HMAC read_ticket

write-file
  ├── проверяет read_ticket
  ├── выбирает expected_sha256
  └── execute_file_write_transaction()

replace-in-file
  ├── вызывает execute_read_file()
  ├── вытаскивает текст из CallToolResult
  ├── парсит публичный JSON через serde_json::Value
  ├── делает exact replacement
  ├── снова отправляет прочитанный текст на remote-хост ради SHA-256
  └── execute_file_write_transaction()
```

Публично эти три инструмента регистрируются в `src/server.rs:675-685`, а маршрутизируются отдельно в `src/server.rs:747-766`.

Сам commit-механизм уже частично вынесен в `file_edit_common.rs`, но он всё ещё возвращает `CallToolResult`, то есть транспортный MCP-тип протекает в доменную логику.

## Самая плохая связность

`replace-in-file` вызывает публичный handler `execute_read_file()`:

```rust
let read_result = self
    .execute_read_file(ReadFileParams { ... })
    .await?;
```

Затем вытаскивает текст из `CallToolResult`, парсит JSON и ищет поля `content` и `sha256`:

* `src/server/handlers/replace_in_file.rs:185-228`;
* похожее повторение есть в `load_remote_text_file_state()`:
  `src/server/handlers/file_edit_common.rs:162-224`.

Это означает, что изменение публичного JSON-контракта `read-file` может сломать внутреннее редактирование на runtime, хотя компилятор ничего не заметит.

Правильная граница:

```text
read-file MCP handler ───────┐
                             ├── RemoteTextStore::read_snapshot()
apply_patch MCP handler ─────┘
```

Оба handler должны зависеть от одного типизированного сервиса, но **не друг от друга**.

---

# Проблемы `read_ticket`

## 1. Ticket подписывает неправильный SHA для preview/head/tail

В документации инструмента написано:

```text
sha256: SHA-256 hex digest of the full file content
```

Это `src/server/tools.rs:89-91`.

Однако фактически `read-file` сначала получает уже усечённый remote producer’ом текст, а затем хеширует именно его:

```rust
let content_sha256 = Sha256::digest(content.as_bytes());
```

См. `src/server/handlers/read_file.rs:237-255`.

После этого этот SHA помещается и в `sha256`, и в ticket:

```rust
result["sha256"] = ...;
result["read_ticket"] = self.ticket_signer.issue(
    &remote_path,
    result["sha256"].as_str().unwrap_or_default(),
    ...
);
```

См. `src/server/handlers/read_file.rs:278-283`.

Следствие: для большого файла стандартный `preview` выдаёт ticket, подписанный SHA первых 800 строк. `write-file` затем сравнивает его с SHA **полного** remote-файла и получает ложный conflict.

Текущие тесты это не ловят, потому что ticket smoke-test работает с небольшим файлом, полностью помещающимся в preview.

## 2. Явный `expected_sha256` отменяет hash-binding ticket

В `write_file.rs` пользовательский SHA имеет приоритет над SHA из ticket:

```rust
match (user_expected_sha256, ticket_bound_sha256) {
    (Some(user), _) => Some(user),
    (None, Some(ticket)) => Some(ticket),
    ...
}
```

См. `src/server/handlers/write_file.rs:70-77`.

То есть можно предъявить ticket от одной версии файла, а в качестве реального optimistic-lock baseline использовать другой SHA. Если цель ticket — доказать, что была прочитана именно перезаписываемая версия, этот контракт уже нарушен.

## 3. Ticket не является границей авторизации

Ticket доказывает только следующее:

> Этот процесс ssh-mcp когда-то подписал сочетание lexical path + SHA + expiry.

Он не доказывает, что:

* модель действительно увидела весь файл;
* модель поняла содержимое;
* пользователь одобрил изменение;
* файл не поменялся после выдачи ticket;
* путь был канонизирован и находится в разрешённом root.

Причём `replace-in-file` проходит эту «защиту» без отдельного чтения моделью: сервер читает файл внутри handler и сразу редактирует его.

Если целью было заставить агента посмотреть файл, `replace-in-file` эту политику обходит. Если целью была защита от race, достаточно нормального внутреннего snapshot + compare-before-commit.

## 4. Ticket создаёт искусственные отказы

Ключ HMAC генерируется на срок жизни процесса:

* `src/ticket.rs:76-92`.

TTL равен десяти минутам:

* `src/ticket.rs:33-34`.

После перезапуска сервера все ticket становятся невалидными, хотя состояние remote-файла могло вообще не измениться. По истечении десяти минут происходит то же самое.

Это не проверка согласованности файла, а дополнительная временная связность между двумя MCP-вызовами.

---

# Что в текущем transaction engine стоит сохранить

Хорошая основа уже есть:

* sibling staging-файл;
* per-file lock через atomic `mkdir`;
* проверка SHA;
* structured markers для conflict;
* `mv` staging-файла на destination;
* cleanup через trap.

Это находится в `src/server/handlers/file_edit_common.rs:480-501`.

Выбрасывать этот слой полностью не нужно. Его нужно превратить из MCP-oriented helper в типизированный `RemoteCommitter`.

Но сначала надо исправить несколько важных свойств.

---

# Текущая «атомарность» слабее, чем заявлено

Сейчас последовательность такая:

```text
1. Захватить ssh-mcp lock.
2. Посчитать SHA destination.
3. Сравнить expected SHA.
4. Передать новый контент по SSH в stage.
5. mv stage destination.
```

В коде SHA проверяется в `file_edit_common.rs:492-494`, а загрузка stage происходит позже, в `495-496`.

Проблема: sidecar-lock вида `.ssh-mcp-lock` соблюдают только другие операции вашего MCP-сервера. Обычный процесс на remote-хосте ничего о нём не знает.

Возможен race:

```text
ssh-mcp: hash(destination) == expected
ssh-mcp: начинает долгую передачу stage

external process: записывает новую версию destination

ssh-mcp: заканчивает передачу
ssh-mcp: mv stage destination
```

Внешнее изменение будет затёрто.

## Правильный порядок

```text
1. Загрузить уникальный stage.
2. Проверить SHA самого stage.
3. Захватить lock.
4. Непосредственно перед finalize проверить destination.
5. Немедленно выполнить rename.
6. Освободить lock.
```

Так lock удерживается недолго, а окно между проверкой destination и rename становится минимальным.

Полностью атомарного content-hash CAS против **не сотрудничающего** внешнего writer’а обычный POSIX rename не предоставляет. Поэтому честные гарантии должны быть такими:

* против других операций ssh-mcp — сериализация lock’ами;
* против изменений, произошедших до финального preflight, — SHA conflict;
* против произвольного внешнего writer’а в микроскопическом окне `hash → rename` — только best effort.

Если нужна строгая гарантия против внешних writers, они должны использовать тот же lock/protocol, либо изменение должно идти через Git, versioned deployment directory, symlink switch или специализированный remote helper.

---

# Ещё три проблемы commit-слоя

## Потеря metadata

Stage создаётся заново:

```sh
: > "$stage"
cat > "$stage"
mv "$stage" "$dst"
```

См. `file_edit_common.rs:495-499`.

После rename destination получает inode и metadata stage-файла. Это означает потенциальную потерю:

* исходного mode;
* ACL;
* xattrs;
* Linux capabilities;
* hard-link identity;
* владельца, если commit выполняется не тем же владельцем;
* некоторых watcher/file-identity semantics.

Минимум перед rename нужно переносить mode. Для owner/group, ACL и xattrs нужна явно выбранная политика:

```rust
pub enum MetadataPolicy {
    PreserveMode,
    PreserveBasic, // mode + uid + gid where permitted
    PreserveStrict,
    Reset,
}
```

Для системных конфигураций я бы использовал `PreserveBasic` по умолчанию и возвращал warning, если ACL/xattrs не удалось сохранить.

## Непоследовательное поведение symlink

Проверка:

```sh
[ -e "$dst" ]
[ -f "$dst" ]
```

следует по symlink к обычному файлу. Но последующий:

```sh
mv "$stage" "$dst"
```

заменяет сам symlink, а не его target.

Получается:

```text
read/hash -> target symlink
commit    -> symlink path
```

Это опасная семантическая рассинхронизация.

Нормальный default:

```text
final symlink: reject
symlink в компонентах пути: reject либо resolve и проверить confinement
```

Режим `follow_symlink=true` можно добавить отдельно, но только если resolved target остаётся внутри разрешённого root.

## Ошибка после фактически выполненного commit

Сейчас новый SHA вычисляется **после** `mv`:

```sh
mv "$stage" "$dst"
new_hash=$(sha256_file "$dst")
```

Если `mv` уже сработал, а последующий hash, SSH-канал или вывод marker’а сломался, клиент получит ошибку, хотя файл был изменён.

Лучше:

1. локально вычислить planned `new_sha256`;
2. проверить SHA stage до rename;
3. после rename считать planned SHA достоверным;
4. при обрыве SSH после возможного rename вернуть:

```json
{
  "error": "outcome_unknown",
  "operation_id": "...",
  "reconcile_required": true
}
```

После этого сервер может перечитать destination и сравнить его с planned SHA.

---

# Как проблему решают другие инструменты

## OpenAI / Codex

Официальный `apply_patch` строится вокруг structured diffs: модель выдаёт операции create/update/delete, а harness применяет их и возвращает результат модели. В shell-формате Codex используется знакомый file-oriented envelope с `*** Begin Patch`, `*** Add File`, `*** Update File`, `*** Delete File` и optional move. ([OpenAI Developers][1])

Сильная сторона подхода — patch одновременно описывает:

* **что** изменить;
* **где** изменить;
* какой старый контекст должен присутствовать.

OpenAI также публикует reference implementation формата без line numbers: расположение изменения определяется контекстом и `@@` anchors. ([OpenAI Developers][2])

Для вашего MCP это лучший внешний формат, но remote transaction engine должен быть собственным. Локальный reference implementation недостаточен для SSH, metadata, symlink и concurrency.

## Aider

Aider пробовал whole-file, SEARCH/REPLACE и упрощённый unified diff. Их вывод: формат должен быть знакомым, простым, без JSON-escaping и без хрупких line numbers. В их историческом benchmark переход от SEARCH/REPLACE к unified-diff-подобному формату поднял результат GPT-4 Turbo с 20% до 61%. ([aider.chat][3])

Для вашего случая отсюда стоит взять:

* знакомую diff-форму;
* минимум структурного JSON;
* context-based addressing;
* отказ от `match_index`.

Но не стоит слепо брать максимально fuzzy matching: для Python, YAML, systemd и shell whitespace иногда семантически важен.

## Anthropic text editor

Anthropic использует отдельные команды `view`, `str_replace`, `create` и `insert`. `str_replace` требует exact match, включая whitespace и indentation, а `insert` адресует позицию line number. ([Claude Platform Docs][4])

Плюсы:

* простая реализация;
* понятный exact-match conflict;
* безопаснее агрессивного fuzzy replacement.

Минусы:

* create/update/insert остаются разными режимами;
* несколько изменений требуют нескольких вызовов;
* line number быстро устаревает;
* модель должна воспроизводить старый текст отдельными JSON-полями.

Ваш текущий `replace-in-file` концептуально близок именно к этому подходу.

## Reference MCP filesystem server

Официальный filesystem MCP по-прежнему разделяет `write_file` и `edit_file`. `edit_file` принимает массив `oldText/newText`, поддерживает dry-run и whitespace normalization. При этом доступ ограничивается заранее разрешёнными директориями или MCP Roots. ([GitHub][5])

Оттуда нужно взять две вещи:

* конфигурируемые allowed roots;
* preview/diff как UX-функцию.

Разделение `write_file`/`edit_file` и обязательный dry-run-круг копировать не стоит.

---

# Рекомендуемый публичный инструмент

Я бы зарегистрировал только каноническое имя:

```text
apply_patch
```

И при необходимости оставил скрытый route alias:

```text
apply-patch
```

В `list_tools` показывать только `apply_patch`: именно это имя уже знакомо моделям по Codex/OpenAI tooling. ([OpenAI Developers][1])

## MCP schema

```rust
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchParams {
    /// Patch in the *** Begin Patch / *** End Patch format.
    pub patch: String,

    /// Configured remote root identifier.
    pub root_id: Option<String>,

    /// Build and return the plan without committing it.
    #[serde(default)]
    pub dry_run: bool,

    /// Optional exact revision guards for selected files.
    #[serde(default)]
    pub expected_sha256: BTreeMap<String, String>,

    pub timeout_ms: Option<u64>,
}
```

Пример вызова:

```json
{
  "root_id": "app",
  "patch": "*** Begin Patch\n*** Update File: config/app.toml\n@@\n-timeout = 10\n+timeout = 30\n*** Add File: config/features.toml\n+experimental = true\n*** Delete File: config/obsolete.toml\n*** End Patch",
  "dry_run": false
}
```

`expected_sha256` здесь:

* прозрачен;
* не подписан HMAC;
* не имеет TTL;
* не обязателен для обычного update;
* используется только когда caller хочет применить patch строго к конкретной ревизии.

Это обычный ETag/precondition, а не ticket.

---

# Paths должны быть относительными разрешённому root

Сейчас `remote_path` должен быть абсолютным, но базовая валидация не запрещает `..`:

* `src/validate.rs:1-33`;
* `src/server/validation/common.rs:45-58`.

Для patch-инструмента не принимайте произвольный абсолютный путь.

Конфигурация:

```text
--edit-root app=/srv/my-app
--edit-root nginx=/etc/nginx
```

Запрос:

```json
{
  "root_id": "nginx",
  "patch": "*** Begin Patch\n*** Update File: nginx.conf\n...\n*** End Patch"
}
```

Проверки `RemotePathResolver`:

```rust
pub struct RemoteEditRoot {
    pub id: String,
    pub absolute_path: String,
}

pub struct RemoteRelativePath(PathBuf);
```

При создании `RemoteRelativePath` запрещать:

* absolute path;
* `..`;
* пустые компоненты;
* NUL/control characters;
* слишком длинные пути;
* path, который после symlink resolution вышел из root.

`root_id` должен выбирать заранее настроенный root. Переданный caller’ом произвольный `"root": "/etc"` не является access control.

---

# Семантика patch-операций

## Add

```text
*** Add File: relative/path
+content
```

Гарантии:

* destination обязан отсутствовать;
* существующий файл никогда не перезаписывается;
* parent должен существовать либо создание родителей должно быть отдельной явной политикой;
* expected state внутри plan: `Missing`.

Для Linux strict create можно реализовать через `renameat2(RENAME_NOREPLACE)` в remote helper либо через атомарный `link(2)` из sibling stage. Обычная последовательность `test ! -e && mv` имеет race.

## Update

```text
*** Update File: relative/path
@@ optional anchor
 context
-old
+new
 context
```

Гарантии:

* source существует;
* это regular UTF-8 file;
* каждый hunk должен найти ровно одно подходящее место;
* hunks применяются по порядку к in-memory document;
* ambiguity — ошибка, а не автоматический выбор первого совпадения;
* internal expected state: SHA снимка, на котором построен plan.

`match_index` больше не нужен. Если контекст неоднозначный, модель должна дать больше контекста или `@@` anchor.

## Delete

```text
*** Delete File: relative/path
```

Гарантии:

* source существует и является regular file;
* internal commit использует SHA прочитанного снимка;
* server policy может требовать пользовательский `expected_sha256`, потому что в самом delete-разделе нет old context.

Практичная политика:

```rust
pub enum DestructivePreconditionPolicy {
    InternalSnapshot,
    RequireExplicitShaForDelete,
    RequireExplicitShaForAllExistingFiles,
}
```

Я бы выбрал `RequireExplicitShaForDelete` для `/etc`-подобных roots и `InternalSnapshot` для project workspace.

## Move

```text
*** Update File: old/path
*** Move to: new/path
```

Гарантии:

* source существует;
* destination отсутствует;
* оба пути находятся в одном configured root;
* destination также входит в lock/preflight set;
* move-to-self и циклы отклоняются.

---

# Какие preconditions реально нужны

Нужно разделить три разных понятия.

## 1. Semantic precondition

Patch-контекст:

```diff
-old_value
+new_value
```

говорит: «в файле должен присутствовать именно этот фрагмент».

Это защищает смысл изменения. Если файл изменился в другом месте, но нужный контекст всё ещё существует, patch можно безопасно применить к актуальной версии.

## 2. Revision precondition

SHA-256 говорит:

> Файл целиком должен быть ровно той версии.

Нужен не всегда. Полезен для:

* delete;
* security-sensitive configs;
* full replacement;
* workflow, где human одобрил diff конкретной версии.

Он может передаваться открыто как `expected_sha256`. HMAC не добавляет корректности.

## 3. Race protection внутри одного вызова

Даже если caller ничего не передал, `apply_patch` должен:

1. прочитать snapshot;
2. вычислить его SHA;
3. построить результат;
4. перед commit снова сравнить remote SHA с SHA snapshot.

Этот hash живёт внутри одного вызова `apply_patch`. Никакого межвызовного ticket не требуется.

---

# Нужный dependency graph

```text
MCP layer
┌──────────────────────────────────────────────┐
│ read_file handler                            │
│ apply_patch handler                          │
│ JSON schema / CallToolResult serialization   │
└──────────────────────┬───────────────────────┘
                       │ typed calls
                       ▼
Application layer
┌──────────────────────────────────────────────┐
│ ApplyPatchService                            │
│   parse -> resolve -> snapshot -> plan        │
│   dry-run OR commit                          │
└───────────────┬───────────────────┬──────────┘
                │                   │
                ▼                   ▼
Pure patch domain              Remote filesystem
┌──────────────────────┐       ┌──────────────────────┐
│ PatchParser          │       │ RemoteTextStore      │
│ PatchMatcher         │       │ RemoteCommitter      │
│ PatchPlanner         │       │ RemotePathResolver   │
│ DiffRenderer         │       │ LockManager          │
└──────────────────────┘       └──────────────────────┘
```

Главное правило:

> Только handler знает о `CallToolResult`, `Content`, `McpError` и публичном JSON.

Parser, planner и remote transaction возвращают нормальные Rust-типы.

---

# Предлагаемая структура модулей

```text
src/
  patch/
    mod.rs
    ast.rs
    parser.rs
    matcher.rs
    planner.rs
    error.rs

  remote_fs/
    mod.rs
    path.rs
    snapshot.rs
    metadata.rs
    transaction.rs
    error.rs

  server/
    handlers/
      read_file.rs
      apply_patch.rs
```

`file_edit_common.rs` можно постепенно разделить между:

```text
remote_fs/snapshot.rs
remote_fs/transaction.rs
```

## Основные типы

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Digest([u8; 32]);

#[derive(Debug, Clone)]
pub enum ExpectedState {
    Missing,
    Sha256(Sha256Digest),
}

#[derive(Debug, Clone)]
pub enum RemotePathKind {
    Missing,
    RegularFile,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct RemoteFileSnapshot {
    pub path: RemotePath,
    pub kind: RemotePathKind,
    pub bytes: Vec<u8>,
    pub sha256: Option<Sha256Digest>,
    pub metadata: Option<RemoteFileMetadata>,
}

#[derive(Debug)]
pub enum PatchOperation {
    Add {
        path: RemoteRelativePath,
        content: Vec<u8>,
    },
    Update {
        path: RemoteRelativePath,
        move_to: Option<RemoteRelativePath>,
        hunks: Vec<PatchHunk>,
    },
    Delete {
        path: RemoteRelativePath,
    },
}

#[derive(Debug)]
pub struct PlannedFileChange {
    pub source: RemotePath,
    pub destination: Option<RemotePath>,
    pub operation: PlannedOperation,
    pub expected_source: ExpectedState,
    pub expected_destination: Option<ExpectedState>,
    pub old_bytes: Option<Vec<u8>>,
    pub new_bytes: Option<Vec<u8>>,
    pub new_sha256: Option<Sha256Digest>,
}
```

Внутри используйте enum `ExpectedState::Missing`, а не строковый all-zero SHA sentinel. Sentinel допустим только как внутренний wire marker shell-протокола.

---

# Handler должен быть почти пустым

```rust
pub async fn execute_apply_patch(
    &self,
    params: ApplyPatchParams,
) -> Result<CallToolResult, McpError> {
    let request = ApplyPatchRequest::try_from(params)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

    match self.patch_service.apply(request).await {
        Ok(report) => Ok(json_success(report)),
        Err(error) => Ok(json_tool_error(error)),
    }
}
```

А orchestration находится в сервисе:

```rust
pub async fn apply(
    &self,
    request: ApplyPatchRequest,
) -> Result<ApplyPatchReport, ApplyPatchError> {
    let patch = self.parser.parse(&request.patch)?;
    let resolved = self.path_resolver.resolve_patch(patch, request.root_id)?;
    let snapshots = self.store.read_snapshots(&resolved).await?;
    let plan = self.planner.build(resolved, snapshots, &request.preconditions)?;

    if request.dry_run {
        return Ok(ApplyPatchReport::from_plan(plan));
    }

    self.committer.commit(plan).await
}
```

Никаких:

* `execute_read_file()` внутри;
* `CallToolResult` внутри planner;
* `serde_json::Value` внутри edit service;
* повторной отправки текста на remote-хост ради SHA.

Локальный SHA UTF-8 bytes и remote SHA этих же bytes одинаковы. `compute_partial_baseline_sha256()` из `file_edit_common.rs:229-299` после рефакторинга не нужен.

---

# Как организовать batch commit

Рекомендованная последовательность:

```text
1. Полностью распарсить patch.
2. Проверить лимиты и path conflicts.
3. Разрешить все пути относительно root.
4. Прочитать все необходимые snapshots.
5. Применить все hunks в памяти.
6. Убедиться, что весь patch планируется без ошибок.
7. Вычислить diff и new SHA.
8. Загрузить все stage-файлы.
9. Захватить все lock’и в сортированном порядке.
10. Повторно проверить все source/destination preconditions.
11. Перенести metadata на stage.
12. Выполнить finalize в детерминированном порядке.
13. Освободить lock’и и удалить staging.
```

## Почему lock’и надо сортировать

Для multi-file patch два параллельных вызова могут брать файлы в разном порядке:

```text
operation A: lock file1, ждёт file2
operation B: lock file2, ждёт file1
```

Все canonical paths нужно сортировать и захватывать в одном порядке.

## Не называйте multi-file commit полностью атомарным

Обычная файловая система не предоставляет единой транзакции сразу для нескольких произвольных файлов.

Честное имя гарантии:

```json
{
  "atomicity": "batch_preflight_per_file_commit"
}
```

Оно означает:

* до первой мутации проверены все файлы;
* каждый отдельный replace атомарен через rename;
* при ошибке finalize выполняется best-effort rollback;
* возможен `partial_apply`, который явно перечисляет состояние каждого пути.

Если пока не готовы реализовать rollback/journal, безопасный первый релиз — разрешить только один затрагиваемый файл на patch:

```text
max_files_per_patch = 1
```

Формат и API при этом уже останутся будущесовместимыми.

---

# Dry-run не должен превращаться в новый ticket

`dry_run` полезен для человека, но не должен быть обязательным первым этапом.

Плохой workflow:

```text
dry_run -> получить plan_token -> apply с plan_token
```

Это тот же ticket, только под другим названием.

Нормальный workflow:

```text
apply_patch(dry_run=true)
  -> вернуть diff + base_sha256

apply_patch(dry_run=false, expected_sha256=base_sha256)
  -> заново прочитать и построить plan
  -> conflict, если версия изменилась
```

Причём `expected_sha256` передаётся только когда caller действительно хочет закрепить preview к конкретной версии. Для обычного автоматического изменения достаточно patch-контекста и внутреннего CAS.

Если нужна approval-система, approval должен быть связан с:

```text
hash(canonical root_id + canonical patch)
```

то есть с намерением изменения, а не с opaque read-ticket. Проверка состояния файлов остаётся отдельным механизмом.

---

# Matching policy

Для remote-конфигов я рекомендую строгий default.

```rust
pub enum MatchPolicy {
    Exact,
    IgnoreTrailingWhitespace,
}
```

В первом релизе можно оставить только `Exact`.

Правила:

* old/context lines сопоставляются буквально;
* line endings обрабатываются отдельно от содержимого строки;
* trailing whitespace не игнорируется без явного режима;
* несколько совпадений — `ambiguous_hunk`;
* отсутствие совпадения — `context_mismatch`;
* hunks применяются последовательно;
* pure insertion обязан иметь anchor/context либо специальный EOF marker;
* никаких автоматических Unicode punctuation substitutions;
* никаких silent first-match replacements.

При ошибке возвращайте bounded diagnostic:

```json
{
  "ok": false,
  "error": "ambiguous_hunk",
  "path": "config/app.toml",
  "hunk": 2,
  "candidate_lines": [18, 57],
  "message": "Hunk context matched more than once; include additional context."
}
```

Это лучше текущего `match_index`, поскольку модель исправляет сам patch, а не угадывает изменчивый порядковый номер совпадения.

---

# Line endings и raw bytes

Snapshot лучше хранить как bytes, даже если инструмент поддерживает только UTF-8:

```rust
pub struct RemoteFileSnapshot {
    pub bytes: Vec<u8>,
    pub text: String,
    pub sha256: Sha256Digest,
}
```

Причины:

* SHA должен считаться по точным remote bytes;
* нужно сохранять CRLF/LF;
* нужно сохранять наличие или отсутствие финального newline;
* diff matcher может работать с logical lines, а writer — с восстановленными bytes.

Для существующего файла inserted lines должны использовать его доминирующий line ending. Для нового файла можно использовать LF.

Binary-файлы по-прежнему должны идти через `transfer`, а `apply_patch` должен возвращать `invalid_utf8`.

---

# Исправление `read-file`

После удаления ticket ответ лучше сделать таким:

```json
{
  "path": "/srv/app/config.toml",
  "mode": "preview",
  "content": "...",
  "content_sha256": "hash-of-returned-window",
  "returned_lines": 800,
  "truncated": true
}
```

Для `mode=full` можно дополнительно вернуть:

```json
{
  "file_sha256": "hash-of-entire-file"
}
```

Не называйте hash усечённого content просто `sha256`, если документация обещает SHA полного файла.

Внутреннее чтение для edit не должно зависеть от `max_output_tokens`: контент не отправляется модели. Нужен отдельный лимит:

```text
max_remote_edit_file_bytes
max_patch_bytes
max_resulting_file_bytes
max_total_patch_bytes
max_files_per_patch
max_hunks_per_file
```

---

# Формат ответа

Успех:

```json
{
  "ok": true,
  "dry_run": false,
  "atomicity": "batch_preflight_per_file_commit",
  "files": [
    {
      "path": "config/app.toml",
      "operation": "update",
      "changed": true,
      "previous_sha256": "…",
      "new_sha256": "…",
      "bytes_written": 1432,
      "metadata_preserved": true
    }
  ],
  "warnings": []
}
```

Conflict:

```json
{
  "ok": false,
  "error": "conflict",
  "path": "config/app.toml",
  "expected_sha256": "…",
  "actual_sha256": "…",
  "retryable": true
}
```

Неопределённый результат после SSH disconnect:

```json
{
  "ok": false,
  "error": "outcome_unknown",
  "path": "config/app.toml",
  "planned_sha256": "…",
  "operation_id": "…",
  "reconcile_required": true
}
```

Ожидаемые domain errors лучше возвращать как `CallToolResult::error` с JSON body. `McpError` оставить для:

* malformed MCP arguments;
* schema violations;
* internal programmer errors;
* transport-level нарушения протокола.

Сейчас `write-file` и `replace-in-file` смешивают `McpError::invalid_params`, текстовые `Error:` и JSON conflict. Новый инструмент должен иметь единый контракт.

---

# Конкретный план миграции по файлам

## 1. Добавить patch domain

```text
src/patch/ast.rs
src/patch/parser.rs
src/patch/matcher.rs
src/patch/planner.rs
src/patch/error.rs
```

Parser и matcher должны быть полностью pure и покрываться обычными unit tests без SSH.

## 2. Извлечь типизированное чтение

Из `execute_read_file()` вынести:

```rust
async fn read_remote_snapshot(
    &self,
    path: &RemotePath,
    limit: usize,
) -> Result<RemoteFileSnapshot, RemoteFsError>;
```

Публичный `read-file` будет только форматировать snapshot/window в JSON.

## 3. Переписать transaction return type

Сейчас:

```rust
execute_file_write_transaction(...)
    -> Result<CallToolResult, McpError>
```

Нужно:

```rust
commit_file(...)
    -> Result<FileCommitResult, RemoteCommitError>
```

MCP-конвертация — только в handler.

## 4. Убрать зависимость edit от `TransferEngine`

Сейчас local staging создаётся через:

```rust
self.transfer.local_root()
```

в `file_edit_common.rs:404-405`.

Это ненужная связность edit → transfer.

При текущем лимите 1 MiB можно стримить:

```rust
let mut input = std::io::Cursor::new(new_bytes);
```

Для больших файлов используйте отдельный `EditSpool`, а не `TransferEngine`.

## 5. Добавить `apply_patch`

Изменения:

```text
src/tools/mod.rs
src/server/tools.rs
src/server/handlers/mod.rs
src/server/handlers/apply_patch.rs
src/server.rs
src/lib.rs
README.md
```

В `list_tools`:

```text
read-file
apply_patch
```

## 6. Спрятать legacy tools

На один переходный релиз можно:

* не показывать `write-file` и `replace-in-file` в `list_tools`;
* продолжать принимать старые route;
* внутри преобразовывать их в typed plan;
* возвращать warning `deprecated_tool`.

Но legacy handler не должен больше вызывать `execute_read_file` и не должен требовать ticket.

## 7. Удалить ticket infrastructure

После удаления compatibility:

* удалить `src/ticket.rs`;
* удалить `ticket_signer` из `SshMcpServer`;
* удалить initialization в `src/server.rs:198`;
* удалить `read_ticket` из `ReadFileResponse` и `WriteFileParams`;
* удалить `hmac` и `getrandom` из `Cargo.toml`;
* оставить `sha2` и `similar`.

Поскольку текущая версия проекта `2.1.0`, удаление публичных инструментов и параметров логично выпустить как `3.0.0`.

---

# Обязательные тесты

До удаления старых инструментов я бы добавил следующие regressions:

1. `read-file preview` большого файла: `content_sha256` не выдаётся за full-file SHA.
2. Update с уникальным context.
3. Update с отсутствующим context.
4. Update с несколькими совпадениями.
5. Add существующего файла не перезаписывает его.
6. Delete с неверным explicit SHA не удаляет файл.
7. Изменение destination во время загрузки stage приводит к conflict.
8. Два multi-file patch захватывают lock’и без deadlock.
9. Один конфликт в batch preflight не изменяет ни одного файла.
10. Mode executable-файла сохраняется.
11. Final symlink отклоняется.
12. `../` и absolute paths отклоняются.
13. Symlink-component не позволяет выйти из configured root.
14. CRLF и отсутствие финального newline сохраняются.
15. `$`, `\`, quotes и Unicode остаются literal bytes.
16. SSH disconnect после rename возвращает `outcome_unknown`, затем reconciliation определяет реальное состояние.
17. Повторное применение того же patch даёт предсказуемый context conflict, а не повторное изменение.
18. Server restart никак не влияет на возможность применить patch к неизменившемуся файлу.

---

# Итоговая рекомендация

Окончательная модель должна выглядеть так:

```text
read-file:
  только чтение и наблюдаемая metadata

apply_patch:
  один self-contained edit request
  patch context = semantic precondition
  optional SHA = exact revision precondition
  internal snapshot SHA = race protection
  no ticket
  no mandatory dry-run
  no handler-to-handler calls
```

Наиболее важные немедленные исправления даже до полного перехода:

1. исправить ложный full-file SHA в `read-file`;
2. перестать разбирать `CallToolResult` внутри edit-кода;
3. перенести загрузку stage до финальной SHA-проверки destination;
4. отклонять symlink;
5. сохранять metadata;
6. ограничить редактирование configured roots;
7. заменить opaque HMAC ticket на внутренний snapshot SHA и optional открытый `expected_sha256`.

Я выполнил статический разбор исходников. Запустить `cargo test --all-targets` в текущем окружении не удалось: исполняемый файл `cargo` отсутствует, поэтому runtime-поведение и компиляцию предложенного рефакторинга здесь не проверял.

[1]: https://developers.openai.com/api/docs/guides/tools-apply-patch "
  Apply Patch | OpenAI API
"
[2]: https://developers.openai.com/cookbook/examples/gpt4-1_prompting_guide "
  GPT-4.1 Prompting Guide
"
[3]: https://aider.chat/docs/unified-diffs.html "Unified diffs make GPT-4 Turbo 3X less lazy | aider"
[4]: https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/text-editor-tool "Text editor tool - Claude Platform Docs"
[5]: https://github.com/modelcontextprotocol/servers/blob/main/src/filesystem/README.md "servers/src/filesystem/README.md at main · modelcontextprotocol/servers · GitHub"
