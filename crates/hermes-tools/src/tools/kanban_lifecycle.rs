//! Tool kanban Worker 1 (Wave 5): board CRUD + lifecycle task.
//!
//! Port dari `hermes-rs` (`kanban/tools.rs` Worker A + `kanban/tools_queries.rs`
//! Worker B), diadaptasi ke pola `ToolHandler` fork ini (lihat `todo.rs`):
//! `pub struct XxxHandler`, `#[async_trait] impl ToolHandler` dengan
//! `fn schema(&self) -> ToolSchema` via `hermes_core::tool_schema` dan
//! `async fn execute(&self, params: Value) -> Result<String, ToolError>`.
//!
//! Nama tool, argumen, dan semantik mengikuti aslinya
//! (`tools/kanban_tools_schemas.py`), termasuk semantik lifecycle:
//! loop-breaker block (eskalasi ke triage), parent-gating, sticky blocks,
//! dan idempotency.

use super::kanban_store::{
    kanban_home, normalize_board_slug, BoardDb, BoardOptions, KanbanStore, NewTask, TaskFilter,
    ALL_STATUSES, STATUS_ARCHIVED, STATUS_BLOCKED, STATUS_DONE, STATUS_READY, STATUS_RUNNING,
    STATUS_SCHEDULED, STATUS_TODO, STATUS_TRIAGE,
};
use async_trait::async_trait;
use hermes_core::{redact_secrets, tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};
use indexmap::IndexMap;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Konstanta (cerminan aslinya)
// ---------------------------------------------------------------------------

/// Jenis block yang sah — cerminan `VALID_BLOCK_KINDS` di aslinya.
const VALID_BLOCK_KINDS: &[&str] = &["dependency", "needs_input", "capability", "transient"];
/// Batas re-block sejenis sebelum eskalasi ke triage — cerminan
/// `BLOCK_RECURRENCE_LIMIT = 2` di aslinya.
const BLOCK_RECURRENCE_LIMIT: i64 = 2;
/// Batas list default/maks — cerminan `KANBAN_LIST_DEFAULT_LIMIT` = 50 dan
/// `KANBAN_LIST_MAX_LIMIT` = 200 di aslinya.
const LIST_DEFAULT_LIMIT: usize = 50;
const LIST_MAX_LIMIT: usize = 200;

// ---------------------------------------------------------------------------
// Helper param & env
// ---------------------------------------------------------------------------

fn invalid(tool: &str, msg: impl Into<String>) -> ToolError {
    ToolError::InvalidParams(format!("{tool}: {}", msg.into()))
}

fn failed(tool: &str, msg: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(format!("{tool}: {}", msg.into()))
}

fn get_str(params: &Value, tool: &str, name: &str) -> Result<String, ToolError> {
    params
        .get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| invalid(tool, format!("parameter '{name}' wajib diisi")))
}

fn opt_str<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(|v| v.as_str())
}

fn opt_bool(params: &Value, name: &str) -> Option<bool> {
    params.get(name).and_then(|v| v.as_bool())
}

fn opt_i64(params: &Value, name: &str) -> Option<i64> {
    params.get(name).and_then(|v| v.as_i64())
}

fn opt_string_array(params: &Value, tool: &str, name: &str) -> Result<Vec<String>, ToolError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| invalid(tool, format!("`{name}` harus array string")))
            })
            .collect(),
        Some(_) => Err(invalid(tool, format!("`{name}` harus array string"))),
    }
}

fn str_prop(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn open_store(tool: &str) -> Result<KanbanStore, ToolError> {
    KanbanStore::open(kanban_home())
        .map_err(|e| failed(tool, format!("gagal membuka kanban home: {e}")))
}

/// Resolusi board ala `_DESC_BOARD` di skema aslinya: argumen `board` →
/// env `HERMES_KANBAN_BOARD` → board aktif (`current`) → `"default"`.
fn resolve_board(params: &Value, store: &KanbanStore, tool: &str) -> Result<String, ToolError> {
    if let Some(b) = opt_str(params, "board").filter(|s| !s.trim().is_empty()) {
        return normalize_board_slug(b)
            .map_err(|e| invalid(tool, format!("board tidak valid: {e}")));
    }
    if let Ok(b) = std::env::var("HERMES_KANBAN_BOARD") {
        if !b.trim().is_empty() {
            return normalize_board_slug(b.trim())
                .map_err(|e| invalid(tool, format!("board tidak valid: {e}")));
        }
    }
    store
        .current_board()
        .map_err(|e| failed(tool, format!("gagal membaca board aktif: {e}")))
}

/// Resolusi task id ala `_DESC_TASK_ID_DEFAULT` aslinya: argumen `task_id` →
/// env `HERMES_KANBAN_TASK` (task milik worker yang di-spawn dispatcher).
fn resolve_task_id(params: &Value, tool: &str) -> Result<String, ToolError> {
    if let Some(t) = opt_str(params, "task_id").filter(|s| !s.is_empty()) {
        return Ok(t.to_string());
    }
    std::env::var("HERMES_KANBAN_TASK")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid(
                tool,
                "task_id wajib diisi (atau set HERMES_KANBAN_TASK); \
                 pakai kanban_list untuk menemukan id",
            )
        })
}

fn board_db_for(params: &Value, tool: &str) -> Result<(BoardDb, String), ToolError> {
    let store = open_store(tool)?;
    let board = resolve_board(params, &store, tool)?;
    let db = store
        .board_db(&board)
        .map_err(|e| failed(tool, format!("gagal membuka board '{board}': {e}")))?;
    Ok((db, board))
}

fn created_by() -> String {
    std::env::var("HERMES_KANBAN_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string())
}

fn board_prop() -> Value {
    str_prop(
        "Slug board kanban target. Bila kosong dipakai HERMES_KANBAN_BOARD, \
         lalu board aktif, lalu 'default'.",
    )
}

fn task_id_prop() -> Value {
    str_prop(
        "Id task. Bila kosong dipakai HERMES_KANBAN_TASK dari env \
         (task milik worker yang di-spawn dispatcher).",
    )
}

/// Redaksi secret pada metadata — cerminan `_redact_metadata` aslinya:
/// serialkan ke JSON, redaksi, parse ulang; bila gagal, pakai nilai asli.
fn redact_metadata(value: &Value) -> Value {
    let raw = serde_json::to_string(value).unwrap_or_default();
    let redacted = redact_secrets(&raw);
    serde_json::from_str(&redacted).unwrap_or_else(|_| value.clone())
}

/// `true` bila semua parent langsung berstatus `done`/`archived`
/// (cerminan `_parents_satisfied` di aslinya; parent yang hilang dihitung
/// belum selesai agar aman).
fn parents_satisfied(db: &BoardDb, task_id: &str, tool: &str) -> Result<bool, ToolError> {
    let parents = db
        .parents_of(task_id)
        .map_err(|e| failed(tool, format!("gagal membaca parent: {e}")))?;
    for p in parents {
        let done = db
            .get_task(&p)
            .map_err(|e| failed(tool, format!("gagal membaca task: {e}")))?
            .map(|t| t.status.as_str() == STATUS_DONE || t.status.as_str() == STATUS_ARCHIVED)
            .unwrap_or(false);
        if !done {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Promosikan task `todo` yang semua parent-nya done/archived menjadi
/// `ready` — cerminan `recompute_ready` aslinya (dipanggil `kanban_list`
/// sebelum listing, seperti CLI). Task `blocked` tidak disentuh: block
/// eksplisit bersifat sticky hingga `kanban_unblock` (cerminan
/// `_has_sticky_block`).
fn recompute_ready(db: &BoardDb, tool: &str) -> Result<usize, ToolError> {
    let todos = db
        .list_tasks(&TaskFilter {
            status: Some(STATUS_TODO.to_string()),
            limit: Some(LIST_MAX_LIMIT),
            ..Default::default()
        })
        .map_err(|e| failed(tool, format!("gagal listing todo: {e}")))?;
    let mut promoted = 0;
    for t in todos {
        if parents_satisfied(db, &t.id, tool)? {
            db.move_task(
                &t.id,
                STATUS_READY,
                Some("recompute_ready: semua parent done"),
            )
            .map_err(|e| failed(tool, format!("gagal promosi: {e}")))?;
            db.record_event(&t.id, "promoted", Some(r#"{"status":"ready"}"#))
                .map_err(|e| failed(tool, format!("gagal mencatat event: {e}")))?;
            promoted += 1;
        }
    }
    Ok(promoted)
}

fn schema_with(
    name: &str,
    description: &str,
    props: Vec<(&str, Value)>,
    required: Vec<&str>,
) -> ToolSchema {
    let mut map = IndexMap::new();
    for (k, v) in props {
        map.insert(k.to_string(), v);
    }
    tool_schema(
        name,
        description,
        JsonSchema::object(map, required.into_iter().map(|s| s.to_string()).collect()),
    )
}

// ---------------------------------------------------------------------------
// kanban_board_create
// ---------------------------------------------------------------------------

/// Membuat board kanban baru (semantik mkdir -p: board yang sudah ada
/// dikembalikan metadatanya).
pub struct KanbanBoardCreateHandler;

#[async_trait]
impl ToolHandler for KanbanBoardCreateHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_board_create";
        let slug = get_str(&params, tool, "slug")?;
        let store = open_store(tool)?;
        let meta = store.create_board(
            &slug,
            BoardOptions {
                name: opt_str(&params, "name").map(|s| s.to_string()),
                description: opt_str(&params, "description").map(|s| s.to_string()),
                icon: opt_str(&params, "icon").map(|s| s.to_string()),
                color: opt_str(&params, "color").map(|s| s.to_string()),
                project_id: None,
            },
        )?;
        Ok(serde_json::to_string_pretty(&meta).unwrap_or_default())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_board_create",
            "Membuat board kanban baru (semantik mkdir -p: board yang sudah ada \
             dikembalikan metadatanya).",
            vec![
                (
                    "slug",
                    str_prop("Slug board, mis. 'proyek-web' (huruf kecil, angka, -, _)."),
                ),
                ("name", str_prop("Nama tampilan board (default = slug).")),
                ("description", str_prop("Deskripsi board.")),
                ("icon", str_prop("Ikon/emoji board.")),
                ("color", str_prop("Warna aksen board.")),
            ],
            vec!["slug"],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_board_list
// ---------------------------------------------------------------------------

/// Daftar metadata semua board kanban ('default' selalu pertama).
pub struct KanbanBoardListHandler;

#[async_trait]
impl ToolHandler for KanbanBoardListHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_board_list";
        let include_archived = opt_bool(&params, "include_archived").unwrap_or(true);
        let store = open_store(tool)?;
        let boards = store.list_boards(include_archived)?;
        Ok(serde_json::to_string_pretty(&boards).unwrap_or_default())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_board_list",
            "Daftar metadata semua board kanban ('default' selalu pertama).",
            vec![(
                "include_archived",
                json!({"type": "boolean",
                       "description": "Sertakan board yang diarsipkan (default true)."}),
            )],
            vec![],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_board_remove
// ---------------------------------------------------------------------------

/// Menghapus board kanban (default: diarsipkan; `archive=false` = permanen).
pub struct KanbanBoardRemoveHandler;

#[async_trait]
impl ToolHandler for KanbanBoardRemoveHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_board_remove";
        let slug = get_str(&params, tool, "slug")?;
        let archive = opt_bool(&params, "archive").unwrap_or(true);
        let store = open_store(tool)?;
        let meta = store.remove_board(&slug, archive)?;
        let action = if archive { "archived" } else { "deleted" };
        Ok(json!({"slug": meta.slug, "action": action}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_board_remove",
            "Menghapus board kanban. Secara default diarsipkan ke \
             `boards/_archived/<slug>-<ts>/`; set `archive=false` untuk hapus \
             permanen. Board 'default' tidak boleh dihapus.",
            vec![
                ("slug", str_prop("Slug board yang dihapus.")),
                (
                    "archive",
                    json!({"type": "boolean",
                           "description": "Arsipkan alih-alih hapus permanen (default true)."}),
                ),
            ],
            vec!["slug"],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_create
// ---------------------------------------------------------------------------

/// Membuat task kanban baru, opsional sebagai child dari task lain.
pub struct KanbanCreateHandler;

#[async_trait]
impl ToolHandler for KanbanCreateHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_create";
        let (db, board) = board_db_for(&params, tool)?;
        let new_task = NewTask {
            title: get_str(&params, tool, "title")?,
            assignee: get_str(&params, tool, "assignee")?,
            body: opt_str(&params, "body").map(|s| s.to_string()),
            parents: opt_string_array(&params, tool, "parents")?,
            tenant: opt_str(&params, "tenant").map(|s| s.to_string()),
            priority: opt_i64(&params, "priority").unwrap_or(0),
            workspace_kind: opt_str(&params, "workspace_kind").map(|s| s.to_string()),
            workspace_path: opt_str(&params, "workspace_path").map(|s| s.to_string()),
            project_id: opt_str(&params, "project").map(|s| s.to_string()),
            triage: opt_bool(&params, "triage").unwrap_or(false),
            idempotency_key: opt_str(&params, "idempotency_key").map(|s| s.to_string()),
            max_runtime_seconds: opt_i64(&params, "max_runtime_seconds"),
            initial_status: opt_str(&params, "initial_status").map(|s| s.to_string()),
            skills: opt_string_array(&params, tool, "skills")?,
            goal_mode: opt_bool(&params, "goal_mode").unwrap_or(false),
            goal_max_turns: opt_i64(&params, "goal_max_turns"),
            model: opt_str(&params, "model").map(|s| s.to_string()),
            provider: opt_str(&params, "provider").map(|s| s.to_string()),
            completion_contract: opt_str(&params, "completion_contract").map(|s| s.to_string()),
            created_by: Some(created_by()),
            session_id: None,
        };
        let task = db.create_task(new_task)?;
        Ok(json!({
            "id": task.id,
            "title": task.title,
            "status": task.status,
            "assignee": task.assignee,
            "board": board,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_create",
            "Membuat task kanban baru, opsional sebagai child dari task lain \
             (isi `parents`). Dipakai orchestrator untuk fan-out: uraikan kerja \
             menjadi child task dengan assignee spesifik. Status awal: `triage` \
             bila `triage=true`, `todo` bila ada parent yang belum done, \
             `blocked` bila `initial_status=blocked`, selain itu `running`.",
            vec![
                ("title", str_prop("Judul singkat task (wajib).")),
                (
                    "assignee",
                    str_prop(
                        "Nama profil yang mengerjakan task (mis. 'researcher-a'). \
                         Wajib — task tanpa assignee tidak pernah di-dispatch.",
                    ),
                ),
                (
                    "body",
                    str_prop("Opening post: spek lengkap, kriteria terima, tautan."),
                ),
                (
                    "parents",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Id task parent. Task baru tetap di 'todo' \
                           hingga semua parent 'done', lalu auto-promote ke 'ready'."}),
                ),
                (
                    "tenant",
                    str_prop("Namespace opsional untuk isolasi multi-proyek."),
                ),
                (
                    "priority",
                    json!({"type": "integer",
                           "description": "Tiebreaker dispatcher; lebih tinggi = diambil lebih dulu."}),
                ),
                (
                    "workspace_kind",
                    json!({"type": "string", "enum": ["scratch", "dir", "worktree"],
                           "description": "Rasa workspace: 'scratch' (default), 'dir' \
                           (butuh workspace_path absolut), 'worktree' (git worktree)."}),
                ),
                (
                    "workspace_path",
                    str_prop("Path absolut untuk workspace 'dir'/'worktree'."),
                ),
                (
                    "project",
                    str_prop("Id/slug proyek untuk menautkan task."),
                ),
                (
                    "triage",
                    json!({"type": "boolean",
                           "description": "True: task mendarat di 'triage', bukan jalur dispatch biasa."}),
                ),
                (
                    "idempotency_key",
                    str_prop("Bila task non-arsip dengan kunci ini sudah ada, \
                     kembalikan id-nya alih-alih membuat duplikat."),
                ),
                (
                    "max_runtime_seconds",
                    json!({"type": "integer", "description": "Batas runtime per task."}),
                ),
                (
                    "initial_status",
                    json!({"type": "string", "enum": ["running", "blocked"],
                           "description": "Status awal kartu. 'blocked' untuk task yang \
                           butuh human ops segera."}),
                ),
                (
                    "skills",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Nama skill yang di-force-load ke worker yang di-dispatch."}),
                ),
                (
                    "goal_mode",
                    json!({"type": "boolean",
                           "description": "Jalankan worker dalam goal loop (default false)."}),
                ),
                (
                    "completion_contract",
                    str_prop("Kontrak publikasi: local-only (default), OWNER/REPO, atau URL PR GitHub."),
                ),
                (
                    "goal_max_turns",
                    json!({"type": "integer",
                           "description": "Budget turn untuk goal_mode (default 20)."}),
                ),
                (
                    "model",
                    str_prop("Pin worker ke model ini (nama persis yang diharapkan provider)."),
                ),
                (
                    "provider",
                    str_prop("Provider pemilik 'model' (mis. 'openrouter', 'anthropic'). \
                     Wajib diisi bila model bukan dari provider default profil; membutuhkan 'model'."),
                ),
                ("board", board_prop()),
            ],
            vec!["title", "assignee"],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_show
// ---------------------------------------------------------------------------

/// Baca state lengkap sebuah task + string `worker_context`.
pub struct KanbanShowHandler;

#[async_trait]
impl ToolHandler for KanbanShowHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_show";
        let task_id = resolve_task_id(&params, tool)?;
        let (db, board) = board_db_for(&params, tool)?;
        let task = db
            .get_task(&task_id)?
            .ok_or_else(|| failed(tool, format!("task '{task_id}' tidak ada")))?;
        let parents = db.parents_of(&task_id)?;
        let children = db.children_of(&task_id)?;
        let comments = db.list_comments(&task_id)?;
        let events = db.list_events(&task_id, 20)?;
        let attachments = db.list_attachments(&task_id)?;

        let worker_context = build_worker_context(&task, &parents, &children, &comments, &events);
        Ok(json!({
            "task": task,
            "board": board,
            "parents": parents,
            "children": children,
            "comments": comments,
            "recent_events": events,
            "attachments": attachments,
            "worker_context": worker_context,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_show",
            "Baca state lengkap sebuah task: judul, body, assignee, status, \
             parent/child, komentar, dan event-event terakhir. Respons memuat \
             string `worker_context` yang siap disisipkan verbatim ke penalaran.",
            vec![("task_id", task_id_prop()), ("board", board_prop())],
            vec![],
        )
    }
}

fn build_worker_context(
    task: &super::kanban_store::KanbanTask,
    parents: &[String],
    children: &[String],
    comments: &[super::kanban_store::Comment],
    events: &[super::kanban_store::TaskEvent],
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "Task {}: {} [{}]", task.id, task.title, task.status);
    if let Some(a) = &task.assignee {
        let _ = writeln!(s, "Assignee: {a}");
    }
    if let Some(b) = &task.body {
        let _ = writeln!(s, "Body:\n{b}");
    }
    if !parents.is_empty() {
        let _ = writeln!(s, "Parents: {}", parents.join(", "));
    }
    if !children.is_empty() {
        let _ = writeln!(s, "Children: {}", children.join(", "));
    }
    if let Some(sum) = &task.summary {
        let _ = writeln!(s, "Last summary: {sum}");
    }
    if !comments.is_empty() {
        let _ = writeln!(s, "Comments ({}):", comments.len());
        for c in comments.iter().take(10) {
            let _ = writeln!(
                s,
                "- [{}] {}: {}",
                c.id,
                c.author,
                truncate_one(&c.body, 300)
            );
        }
    }
    if !events.is_empty() {
        let _ = writeln!(s, "Recent events:");
        for e in events.iter().take(10) {
            let _ = writeln!(s, "- {} {}", e.kind, e.payload.as_deref().unwrap_or(""));
        }
    }
    s
}

fn truncate_one(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// kanban_list
// ---------------------------------------------------------------------------

/// Daftar ringkasan task kanban dengan filter ala CLI + recompute ready.
pub struct KanbanListHandler;

#[async_trait]
impl ToolHandler for KanbanListHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_list";
        let limit = match params.get("limit") {
            None | Some(Value::Null) => LIST_DEFAULT_LIMIT,
            Some(v) => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| invalid(tool, "limit harus integer"))?;
                if !(1..=LIST_MAX_LIMIT as i64).contains(&n) {
                    return Err(invalid(tool, format!("limit harus 1..={LIST_MAX_LIMIT}")));
                }
                n as usize
            }
        };
        if let Some(s) = opt_str(&params, "status") {
            if !ALL_STATUSES.contains(&s) {
                return Err(invalid(tool, format!("status tidak dikenal: '{s}'")));
            }
        }
        let (db, _board) = board_db_for(&params, tool)?;
        // Samakan dengan CLI: dependensi yang lunas sejak tick dispatcher
        // terakhir langsung terlihat orchestrator.
        let promoted = recompute_ready(&db, tool)?;
        // Satu baris ekstra agar output bisa melaporkan truncation.
        let mut rows = db.list_tasks(&TaskFilter {
            assignee: opt_str(&params, "assignee").map(|s| s.to_string()),
            status: opt_str(&params, "status").map(|s| s.to_string()),
            tenant: opt_str(&params, "tenant").map(|s| s.to_string()),
            include_archived: opt_bool(&params, "include_archived").unwrap_or(false),
            limit: Some(limit + 1),
        })?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        let next_limit = if truncated && limit < LIST_MAX_LIMIT {
            Some((limit * 2).min(LIST_MAX_LIMIT))
        } else {
            None
        };
        Ok(json!({
            "tasks": rows,
            "count": rows.len(),
            "limit": limit,
            "truncated": truncated,
            "next_limit": next_limit,
            "promoted": promoted,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_list",
            "Daftar ringkasan task kanban agar orchestrator bisa menemukan kerja \
             untuk di-route. Filter ala CLI: assignee, status, tenant, \
             include_archived, limit. Menghitung ulang task ready sebelum listing \
             (seperti CLI). Baris ringkas memuat id, judul, status, assignee, \
             priority, parent/child, dan jumlah komentar & attachment. Default 50 \
             baris, maks 200, dengan metadata truncation.",
            vec![
                ("assignee", str_prop("Filter assignee/profil opsional.")),
                (
                    "status",
                    json!({"type": "string",
                           "enum": ["triage", "todo", "scheduled", "ready",
                                    "running", "blocked", "review", "done", "archived"],
                           "description": "Filter status task opsional."}),
                ),
                (
                    "tenant",
                    str_prop("Filter namespace tenant/proyek opsional."),
                ),
                (
                    "include_archived",
                    json!({"type": "boolean",
                           "description": "Sertakan task yang diarsipkan (default false)."}),
                ),
                (
                    "limit",
                    json!({"type": "integer",
                           "description": "Maksimum baris yang dikembalikan (default 50, maks 200)."}),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_complete
// ---------------------------------------------------------------------------

/// Tandai task done dengan handoff terstruktur (summary/result wajib).
pub struct KanbanCompleteHandler;

#[async_trait]
impl ToolHandler for KanbanCompleteHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_complete";
        let tid = resolve_task_id(&params, tool)?;
        let summary_raw = opt_str(&params, "summary")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let result_raw = opt_str(&params, "result")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (db, _board) = board_db_for(&params, tool)?;
        let task = db
            .get_task(&tid)?
            .ok_or_else(|| failed(tool, format!("task '{tid}' tidak ada")))?;
        // Bukti penyelesaian: argumen, atau yang sudah tersimpan di kartu
        // (cerminan `_gate_empty_completion` di aslinya).
        let stored_evidence = task
            .summary
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
            || task
                .result
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if summary_raw.is_none() && result_raw.is_none() && !stored_evidence {
            return Err(failed(
                tool,
                "kanban_complete diblokir: berikan 'summary' atau 'result' yang \
                 tidak kosong tentang apa yang dikerjakan. Task masih in-flight \
                 (tidak ada perubahan state).",
            ));
        }
        // `metadata` harus object bila diberikan.
        let mut meta_map = match params.get("metadata") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(_)) => {
                let redacted = redact_metadata(params.get("metadata").unwrap_or(&Value::Null));
                redacted.as_object().cloned().unwrap_or_default()
            }
            Some(_) => return Err(invalid(tool, "'metadata' harus berupa object")),
        };
        // `created_cards`: verifikasi keberadaan tiap id (cek phantom —
        // cerminan `HallucinatedCardsError` di aslinya).
        let created_cards = opt_string_array(&params, tool, "created_cards")?;
        let mut phantom = vec![];
        for c in &created_cards {
            if db.get_task(c)?.is_none() {
                phantom.push(c.clone());
            }
        }
        if !phantom.is_empty() {
            return Err(failed(
                tool,
                format!(
                    "kanban_complete diblokir: created_cards berikut tidak ada: {}. \
                     Task masih in-flight (tidak ada perubahan state). Ulangi \
                     kanban_complete dengan summary/metadata yang sama dan buang \
                     id tersebut dari created_cards.",
                    phantom.join(", ")
                ),
            ));
        }
        // `artifacts`: tiap path harus ada di disk (cerminan
        // `ArtifactPreservationError` — task tetap in-flight).
        let artifacts = opt_string_array(&params, tool, "artifacts")?;
        let missing: Vec<&String> = artifacts
            .iter()
            .filter(|a| !std::path::Path::new(a).exists())
            .collect();
        if !missing.is_empty() {
            return Err(failed(
                tool,
                format!(
                    "kanban_complete tidak bisa mengawetkan artifact: path tidak \
                     ada di disk: {}. Task masih in-flight dan workspace \
                     dipertahankan. Perbaiki path-nya lalu ulangi kanban_complete \
                     dengan handoff yang sama.",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if !artifacts.is_empty() {
            meta_map.insert("artifacts".to_string(), json!(artifacts));
        }
        // Parent harus terminal dulu (cerminan `complete_task` aslinya yang
        // mengembalikan False bila parent belum selesai).
        let mut blockers = vec![];
        for p in db.parents_of(&tid)? {
            match db.get_task(&p)? {
                Some(pt)
                    if pt.status.as_str() == STATUS_DONE
                        || pt.status.as_str() == STATUS_ARCHIVED => {}
                Some(pt) => blockers.push(format!("{p} ({})", pt.status)),
                None => blockers.push(format!("{p} (hilang)")),
            }
        }
        if !blockers.is_empty() {
            return Err(failed(
                tool,
                format!(
                    "tidak bisa complete {tid}: parent belum selesai: {}; \
                     selesaikan parent dulu (done atau archived)",
                    blockers.join(", ")
                ),
            ));
        }
        let metadata_json = if meta_map.is_empty() {
            None
        } else {
            Some(Value::Object(meta_map).to_string())
        };
        let summary = summary_raw.map(redact_secrets);
        let result = result_raw.map(redact_secrets);
        db.complete_task(
            &tid,
            summary.as_deref(),
            metadata_json.as_deref(),
            result.as_deref(),
        )?;
        // Anak yang parent-nya kini semua done ikut dipromosikan
        // (dependency lane aslinya: auto-resume saat parent selesai).
        let promoted = db.promote_ready_children(&tid)?;
        let attachments = db.list_attachments(&tid)?;
        Ok(json!({
            "task_id": tid,
            "status": STATUS_DONE,
            "promoted_children": promoted,
            "attachments": attachments,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_complete",
            "Tandai task saat ini done dengan handoff terstruktur untuk worker \
             dan manusia downstream. Utamakan `summary`: deskripsi 1-3 kalimat \
             yang human-readable; fakta machine-readable (changed_files, \
             tests_run, decisions, findings) di `metadata`. Wajib isi salah satu \
             dari `summary`/`result`. Bila membuat task via `kanban_create` \
             selama run, cantumkan id-nya di `created_cards` — keberadaan tiap id \
             diverifikasi; id phantom memblokir completion. Cantumkan path absolut \
             deliverable di `artifacts` — tiap path harus ada di disk saat \
             completion dan digabung ke metadata.",
            vec![
                ("task_id", task_id_prop()),
                (
                    "summary",
                    str_prop(
                        "Handoff human-readable, 1-3 kalimat. Muncul di Run \
                         History dashboard dan konteks worker downstream.",
                    ),
                ),
                (
                    "metadata",
                    json!({"type": "object",
                           "description": "Dict fakta terstruktur tentang attempt ini, \
                           mis. {\"changed_files\": [...], \"tests_run\": 12}. \
                           Disertakan ke worker downstream bersama summary."}),
                ),
                (
                    "result",
                    str_prop(
                        "Satu baris log hasil (field legacy, dipetakan ke \
                         task.result). Utamakan `summary` bila memungkinkan.",
                    ),
                ),
                (
                    "created_cards",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Manifest id task yang dibuat via \
                           kanban_create selama run ini. Tiap id diverifikasi \
                           keberadaannya; id phantom memblokir completion. \
                           Kosongkan bila tidak membuat kartu."}),
                ),
                (
                    "artifacts",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Path absolut file deliverable yang \
                           dihasilkan selama run (grafik, PDF, spreadsheet, \
                           gambar). Tiap path harus ada di disk saat \
                           completion; path yang hilang membuat task tetap \
                           in-flight agar bisa diperbaiki lalu dicoba lagi."}),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_block
// ---------------------------------------------------------------------------

/// Hentikan kerja pada task dan route sesuai KENAPA stuck (+ loop-breaker).
pub struct KanbanBlockHandler;

#[async_trait]
impl ToolHandler for KanbanBlockHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_block";
        let tid = resolve_task_id(&params, tool)?;
        let reason_raw = opt_str(&params, "reason").map(str::trim).unwrap_or("");
        if reason_raw.is_empty() {
            return Err(invalid(
                tool,
                "reason wajib diisi — jelaskan input apa yang dibutuhkan",
            ));
        }
        let kind_arg = opt_str(&params, "kind");
        if let Some(k) = kind_arg {
            if !VALID_BLOCK_KINDS.contains(&k) {
                return Err(invalid(
                    tool,
                    format!("kind harus salah satu dari {VALID_BLOCK_KINDS:?} (atau kosongkan)"),
                ));
            }
        }
        let (db, _board) = board_db_for(&params, tool)?;
        let task = db
            .get_task(&tid)?
            .ok_or_else(|| failed(tool, format!("task '{tid}' tidak ada")))?;
        if task.status.as_str() != STATUS_RUNNING && task.status.as_str() != STATUS_READY {
            return Err(failed(
                tool,
                format!("tidak bisa block {tid} (id tidak dikenal atau bukan running/ready)"),
            ));
        }
        let reason = redact_secrets(reason_raw);
        // `dependency` hanya menunggu parent yang belum selesai. Tanpa parent
        // terbuka, parkir di todo akan di-respawn context-free oleh
        // recompute_ready — re-kind menjadi needs_input yang sticky
        // (cerminan `block_task` di aslinya).
        let mut kind = kind_arg;
        let mut extra_note: Option<&str> = None;
        let mut requested_kind: Option<&str> = None;
        if kind == Some("dependency") && parents_satisfied(&db, &tid, tool)? {
            kind = Some("needs_input");
            requested_kind = Some("dependency");
            extra_note = Some(
                "kind='dependency' hanya menunggu parent yang belum selesai; \
                 tidak ada parent terbuka, jadi dicatat sebagai needs_input \
                 (sticky hingga manusia unblock) alih-alih parkir di todo.",
            );
        }
        // Routing block (cerminan `_route_block`): dependency → todo tanpa
        // menyentuh recurrences; selainnya menghitung re-block sejenis dan
        // dieskalasi ke triage saat mencapai batas.
        let prev_kind = task.block_kind.as_deref();
        let (new_status, event_kind, recurrences) = if kind == Some("dependency") {
            (STATUS_TODO, "dependency_wait", task.block_recurrences)
        } else {
            let rec = if prev_kind == kind {
                task.block_recurrences + 1
            } else {
                1
            };
            if rec >= BLOCK_RECURRENCE_LIMIT {
                (STATUS_TRIAGE, "block_loop_detected", rec)
            } else {
                (STATUS_BLOCKED, "blocked", rec)
            }
        };
        db.set_block_state(&tid, kind, recurrences)?;
        db.move_task(&tid, new_status, Some(&reason))?;
        let mut payload = json!({
            "reason": reason,
            "kind": kind,
            "source_status": task.status,
        });
        if event_kind != "dependency_wait" {
            payload["recurrences"] = json!(recurrences);
        }
        if event_kind == "block_loop_detected" {
            payload["limit"] = json!(BLOCK_RECURRENCE_LIMIT);
        }
        db.record_event(&tid, event_kind, Some(&payload.to_string()))?;
        let mut out = json!({
            "task_id": tid,
            "status": new_status,
            "block_kind": kind,
            "block_recurrences": recurrences,
        });
        if let Some(n) = extra_note {
            out["note"] = json!(n);
        }
        if let Some(r) = requested_kind {
            out["requested_kind"] = json!(r);
        }
        Ok(out.to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_block",
            "Hentikan kerja pada task dan route sesuai KENAPA stuck. `kind`: \
             'dependency' (menunggu task lain — parkir di todo dan auto-resume \
             saat parent selesai, tanpa manusia; bila tidak ada parent terbuka \
             dicatat sebagai needs_input), 'needs_input' (butuh keputusan/jawaban \
             manusia), 'capability' (tembok keras: tak ada akses/kredensial), atau \
             'transient' (gagal flaky yang mungkin pulih). `reason` tampil ke \
             manusia di board. Task yang di-unblock lalu di-block lagi untuk \
             alasan yang sama akan dieskalasi otomatis ke triage.",
            vec![
                ("task_id", task_id_prop()),
                (
                    "reason",
                    str_prop(
                        "Apa yang dibutuhkan / apa yang menghentikanmu, satu \
                         dua kalimat. Tampil ke manusia di board.",
                    ),
                ),
                (
                    "kind",
                    json!({"type": "string",
                           "enum": ["dependency", "needs_input", "capability", "transient"],
                           "description": "Kenapa blocked. 'dependency' menunggu \
                           di todo dan lanjut otomatis saat parent yang belum \
                           selesai tuntas; bila tidak ada parent terbuka \
                           dicatat sebagai needs_input. Kosongkan bila tidak \
                           ada yang cocok."}),
                ),
                ("board", board_prop()),
            ],
            vec!["reason"],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_unblock
// ---------------------------------------------------------------------------

/// Buka blockir task; re-gate ke parent (ready bila semua done, else todo).
pub struct KanbanUnblockHandler;

#[async_trait]
impl ToolHandler for KanbanUnblockHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_unblock";
        let tid = get_str(&params, tool, "task_id")?;
        let (db, _board) = board_db_for(&params, tool)?;
        let task = db.get_task(&tid)?.ok_or_else(|| {
            failed(
                tool,
                format!("tidak bisa unblock {tid} (tidak blocked atau tidak dikenal)"),
            )
        })?;
        if task.status.as_str() != STATUS_BLOCKED && task.status.as_str() != STATUS_SCHEDULED {
            return Err(failed(
                tool,
                format!("tidak bisa unblock {tid} (tidak blocked atau tidak dikenal)"),
            ));
        }
        // Re-gate ke parent sebelum kembali ke pool (cerminan
        // `_landing_status_after_parents`): ready bila semua parent terminal,
        // todo selama ada yang terbuka.
        let landing = if parents_satisfied(&db, &tid, tool)? {
            STATUS_READY
        } else {
            STATUS_TODO
        };
        // `block_kind`/`block_recurrences` sengaja dipertahankan melewati
        // unblock (cerminan aslinya): me-reset-nya adalah amnesia yang membuat
        // loop unblock <-> re-block tak terbatas; hanya kanban_complete yang
        // membersihkannya.
        db.move_task(&tid, landing, Some("unblocked"))?;
        db.record_event(
            &tid,
            "unblocked",
            Some(&json!({"from": task.status, "to": landing}).to_string()),
        )?;
        Ok(json!({"task_id": tid, "status": landing}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_unblock",
            "Buka blockir task kanban. Pindah ke ready bila semua parent done, \
             atau todo selama ada parent yang masih terbuka.",
            vec![
                (
                    "task_id",
                    str_prop("Id task yang blocked untuk dipindah ke ready/todo."),
                ),
                ("board", board_prop()),
            ],
            vec!["task_id"],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_schedule
// ---------------------------------------------------------------------------

/// Parkir task di `scheduled` selagi menunggu waktu/event eksternal.
pub struct KanbanScheduleHandler;

#[async_trait]
impl ToolHandler for KanbanScheduleHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_schedule";
        let tid = resolve_task_id(&params, tool)?;
        let reason = opt_str(&params, "reason")
            .map(|s| redact_secrets(s.trim()))
            .filter(|s| !s.is_empty());
        let (db, _board) = board_db_for(&params, tool)?;
        if let Err(e) = db.move_task(&tid, STATUS_SCHEDULED, reason.as_deref()) {
            return Err(failed(
                tool,
                format!(
                    "tidak bisa schedule '{tid}' (id tidak dikenal atau bukan \
                     todo/ready/running/blocked): {e}",
                ),
            ));
        }
        db.record_event(
            &tid,
            "scheduled",
            Some(&json!({"reason": reason}).to_string()),
        )?;
        Ok(json!({"task_id": tid, "status": STATUS_SCHEDULED, "reason": reason}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_schedule",
            "Parkir task saat ini di status 'scheduled' selagi menunggu waktu atau \
             event eksternal. Ini mengakhiri run saat ini dan membuat task tidak \
             bisa di-dispatch hingga orchestrator unblock; tidak membuat timer. \
             Taruh penanda bangun seperti SCHEDULED_UNTIL=<ISO8601> di `reason`.",
            vec![
                ("task_id", task_id_prop()),
                (
                    "reason",
                    str_prop(
                        "Alasan opsional atau penanda bangun machine-readable \
                         yang dicatat pada run yang selesai dan event scheduled.",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_heartbeat
// ---------------------------------------------------------------------------

/// Sinyal liveness selama operasi panjang.
pub struct KanbanHeartbeatHandler;

#[async_trait]
impl ToolHandler for KanbanHeartbeatHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_heartbeat";
        let tid = resolve_task_id(&params, tool)?;
        let note = opt_str(&params, "note")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let (db, _board) = board_db_for(&params, tool)?;
        db.heartbeat(&tid, note.as_deref())?;
        Ok(json!({"task_id": tid}).to_string())
    }

    fn schema(&self) -> ToolSchema {
        schema_with(
            "kanban_heartbeat",
            "Sinyal bahwa kamu masih hidup selama operasi panjang (training, \
             encoding, crawl besar). Panggil tiap beberapa menit agar manusia \
             melihat liveness. Murni side effect — tidak mengubah state kerja.",
            vec![
                ("task_id", task_id_prop()),
                (
                    "note",
                    str_prop(
                        "Catatan singkat opsional tentang progres saat ini. \
                         Muncul di event log.",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::kanban_store::{BoardDb, KanbanStore, NewTask, TEST_ENV_LOCK};
    use std::path::PathBuf;

    /// Home kanban terisolasi per-tes: mengunci env proses, menyetel
    /// `HERMES_KANBAN_HOME` ke tmp dir, membersihkan saat drop.
    struct TestHome {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TestHome {
        fn new(tag: &str) -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap();
            let dir = std::env::temp_dir().join(format!(
                "hermes-kanban-lc-test-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::env::set_var(
                "HERMES_KANBAN_HOME",
                dir.join("kanban").to_string_lossy().into_owned(),
            );
            std::env::remove_var("HERMES_KANBAN_BOARD");
            std::env::remove_var("HERMES_KANBAN_TASK");
            Self { dir, _guard: guard }
        }

        fn db(&self) -> BoardDb {
            KanbanStore::open(kanban_home())
                .unwrap()
                .board_db("default")
                .unwrap()
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            std::env::remove_var("HERMES_KANBAN_HOME");
            std::env::remove_var("HERMES_KANBAN_BOARD");
            std::env::remove_var("HERMES_KANBAN_TASK");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn mk(db: &BoardDb, title: &str, parents: Vec<String>) -> String {
        db.create_task(NewTask {
            title: title.into(),
            assignee: "worker".into(),
            parents,
            ..Default::default()
        })
        .unwrap()
        .id
    }

    fn run_json(handler: &impl ToolHandler, params: Value) -> Value {
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(handler.execute(params))
            .unwrap();
        serde_json::from_str(&out).unwrap_or(json!({"raw": out}))
    }

    #[test]
    fn all_schemas_have_kanban_names() {
        let handlers: Vec<(&str, Box<dyn ToolHandler>)> = vec![
            ("kanban_board_create", Box::new(KanbanBoardCreateHandler)),
            ("kanban_board_list", Box::new(KanbanBoardListHandler)),
            ("kanban_board_remove", Box::new(KanbanBoardRemoveHandler)),
            ("kanban_create", Box::new(KanbanCreateHandler)),
            ("kanban_show", Box::new(KanbanShowHandler)),
            ("kanban_list", Box::new(KanbanListHandler)),
            ("kanban_complete", Box::new(KanbanCompleteHandler)),
            ("kanban_block", Box::new(KanbanBlockHandler)),
            ("kanban_unblock", Box::new(KanbanUnblockHandler)),
            ("kanban_schedule", Box::new(KanbanScheduleHandler)),
            ("kanban_heartbeat", Box::new(KanbanHeartbeatHandler)),
        ];
        assert_eq!(handlers.len(), 11);
        for (name, h) in handlers {
            let s = h.schema();
            assert_eq!(s.name, name, "nama schema salah");
            assert!(!s.description.is_empty());
        }
    }

    #[test]
    fn board_tools_roundtrip() {
        let _home = TestHome::new("board");
        let create = KanbanBoardCreateHandler;
        let list = KanbanBoardListHandler;
        let remove = KanbanBoardRemoveHandler;

        let out = run_json(&create, json!({"slug": "web", "name": "Web"}));
        assert_eq!(out["slug"], "web");
        assert_eq!(out["name"], "Web");
        // Idempoten: buat ulang slug yang sama.
        let out2 = run_json(&create, json!({"slug": "web"}));
        assert_eq!(out2["slug"], "web");

        let out = run_json(&list, json!({}));
        let slugs: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs[0], "default");
        assert!(slugs.contains(&"web"));

        // Slug tidak valid ditolak.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt
            .block_on(create.execute(json!({"slug": "tidak valid!"})))
            .is_err());

        // Board default dilindungi.
        assert!(rt
            .block_on(remove.execute(json!({"slug": "default"})))
            .is_err());

        let out = run_json(&remove, json!({"slug": "web"}));
        assert_eq!(out["action"], "archived");
        let out = run_json(&list, json!({"include_archived": false}));
        let slugs: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["slug"].as_str().unwrap())
            .collect();
        assert!(!slugs.contains(&"web"));
    }

    #[test]
    fn kanban_create_and_show() {
        let _home = TestHome::new("create-show");
        let create = KanbanCreateHandler;
        let show = KanbanShowHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Argumen wajib.
        assert!(rt
            .block_on(create.execute(json!({"title": "tanpa assignee"})))
            .is_err());

        let parent = run_json(
            &create,
            json!({"title": "riset", "assignee": "researcher-a", "priority": 5,
                   "body": "kumpulkan data", "tenant": "t1"}),
        );
        let pid = parent["id"].as_str().unwrap().to_string();
        assert!(pid.starts_with("t_"));
        assert_eq!(parent["status"], "running");

        // Child dengan parent → todo.
        let child = run_json(
            &create,
            json!({"title": "sintesis", "assignee": "writer",
                   "parents": [pid], "idempotency_key": "sintesis-1"}),
        );
        assert_eq!(child["status"], "todo");
        let cid = child["id"].as_str().unwrap().to_string();

        // Idempotency: kunci sama → id sama.
        let child2 = run_json(
            &create,
            json!({"title": "sintesis", "assignee": "writer",
                   "parents": [pid], "idempotency_key": "sintesis-1"}),
        );
        assert_eq!(child2["id"].as_str().unwrap(), cid);

        // kanban_show memuat state lengkap + worker_context.
        let shown = run_json(&show, json!({"task_id": cid}));
        assert_eq!(shown["task"]["title"], "sintesis");
        assert_eq!(shown["parents"].as_array().unwrap().len(), 1);
        assert!(shown["worker_context"]
            .as_str()
            .unwrap()
            .contains("sintesis"));

        // Task fiktif → error.
        assert!(rt
            .block_on(show.execute(json!({"task_id": "t_tidakada"})))
            .is_err());

        // Board override via argumen.
        let other = run_json(
            &create,
            json!({"title": "di board lain", "assignee": "w", "board": "lain"}),
        );
        assert_eq!(other["board"], "lain");
        let shown = run_json(&show, json!({"task_id": other["id"], "board": "lain"}));
        assert_eq!(shown["task"]["title"], "di board lain");
    }

    #[test]
    fn kanban_create_validates_workspace_and_provider() {
        let _home = TestHome::new("create-valid");
        let create = KanbanCreateHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // workspace_kind fiktif.
        assert!(rt
            .block_on(
                create.execute(json!({"title": "x", "assignee": "w", "workspace_kind": "awan"}))
            )
            .is_err());
        // provider tanpa model.
        assert!(rt
            .block_on(
                create.execute(json!({"title": "x", "assignee": "w", "provider": "anthropic"}))
            )
            .is_err());
        // triage → triage.
        let out = run_json(
            &create,
            json!({"title": "spek dulu", "assignee": "w", "triage": true}),
        );
        assert_eq!(out["status"], "triage");
    }

    #[test]
    fn show_defaults_to_env_task() {
        let _home = TestHome::new("show-env");
        let create = KanbanCreateHandler;
        let show = KanbanShowHandler;
        let out = run_json(&create, json!({"title": "env task", "assignee": "w"}));
        let id = out["id"].as_str().unwrap().to_string();
        std::env::set_var("HERMES_KANBAN_TASK", &id);
        // Tanpa task_id eksplisit → ambil dari env.
        let shown = run_json(&show, json!({}));
        assert_eq!(shown["task"]["id"].as_str().unwrap(), id);
    }

    #[test]
    fn list_promotes_ready_and_paginates() {
        let home = TestHome::new("list");
        let db = home.db();
        let list = KanbanListHandler;
        let parent = mk(&db, "parent", vec![]);
        let child = mk(&db, "child", vec![parent.clone()]);
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_TODO);
        db.complete_task(&parent, Some("selesai"), None, None)
            .unwrap();

        let out = run_json(&list, json!({}));
        assert_eq!(out["promoted"], 1);
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_READY);
        assert!(out["count"].as_u64().unwrap() >= 2);

        // Paginasi: limit=1 → truncated + next_limit.
        let out = run_json(&list, json!({"limit": 1}));
        assert_eq!(out["count"], 1);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["next_limit"], 2);

        // Validasi argumen.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt.block_on(list.execute(json!({"limit": 0}))).is_err());
        assert!(rt.block_on(list.execute(json!({"limit": 201}))).is_err());
        assert!(rt
            .block_on(list.execute(json!({"limit": "banyak"})))
            .is_err());
        assert!(rt
            .block_on(list.execute(json!({"status": "mengambang"})))
            .is_err());

        // Filter status.
        let out = run_json(&list, json!({"status": "ready"}));
        let ids: Vec<&str> = out["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&child.as_str()));
    }

    #[test]
    fn complete_handoff_promotes_children() {
        let home = TestHome::new("complete");
        let db = home.db();
        let complete = KanbanCompleteHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);

        // Tanpa summary/result dan tanpa bukti tersimpan → ditolak.
        assert!(rt
            .block_on(complete.execute(json!({"task_id": parent})))
            .is_err());
        assert_eq!(
            db.get_task(&parent).unwrap().unwrap().status,
            STATUS_RUNNING
        );

        // created_cards phantom → ditolak.
        assert!(rt
            .block_on(complete.execute(
                json!({"task_id": parent, "summary": "ok", "created_cards": ["t_hantu123"]})
            ))
            .is_err());

        // Artifact hilang → ditolak, task tetap in-flight.
        assert!(rt
            .block_on(complete.execute(json!({"task_id": parent, "summary": "ok",
                       "artifacts": ["/tmp/hermes-tidak-ada-xyz.pdf"]})))
            .is_err());
        assert_eq!(
            db.get_task(&parent).unwrap().unwrap().status,
            STATUS_RUNNING
        );

        // Handoff valid: summary + metadata + artifact nyata.
        let art = std::env::temp_dir().join("hermes-artifact-ok.txt");
        std::fs::write(&art, "deliverable").unwrap();
        let out = run_json(
            &complete,
            json!({"task_id": parent, "summary": "pekerjaan selesai",
                   "metadata": {"tests_run": 3},
                   "artifacts": [art.to_string_lossy()]}),
        );
        assert_eq!(out["task_id"], parent);
        assert_eq!(out["status"], "done");
        assert_eq!(out["promoted_children"][0], child);
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_READY);
        let _ = std::fs::remove_file(&art);

        // Metadata tersimpan sebagai JSON dan memuat artifacts.
        let t = db.get_task(&parent).unwrap().unwrap();
        let meta: Value = serde_json::from_str(t.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["tests_run"], 3);
        assert_eq!(meta["artifacts"][0], art.to_string_lossy().as_ref());
    }

    #[test]
    fn complete_refuses_unsatisfied_parents() {
        let home = TestHome::new("complete-parents");
        let db = home.db();
        let complete = KanbanCompleteHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        let err = rt
            .block_on(complete.execute(json!({"task_id": child, "summary": "coba"})))
            .unwrap_err();
        assert!(
            err.to_string().contains("belum selesai"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn block_routing_and_loop_breaker() {
        let home = TestHome::new("block");
        let db = home.db();
        let block = KanbanBlockHandler;
        let unblock = KanbanUnblockHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // kind=dependency dengan parent terbuka → parkir di todo.
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        db.move_task(&child, STATUS_READY, None).unwrap(); // simulasi dispatch
        let out = run_json(
            &block,
            json!({"task_id": child, "reason": "tunggu induk", "kind": "dependency"}),
        );
        assert_eq!(out["status"], "todo");
        assert_eq!(out["block_kind"], "dependency");

        // kind=dependency tanpa parent → re-kind needs_input (sticky blocked).
        let solo = mk(&db, "solo", vec![]);
        let out = run_json(
            &block,
            json!({"task_id": solo, "reason": "tunggu data", "kind": "dependency"}),
        );
        assert_eq!(out["status"], "blocked");
        assert_eq!(out["block_kind"], "needs_input");
        assert_eq!(out["requested_kind"], "dependency");
        assert!(out["note"].as_str().unwrap().contains("sticky"));

        // Loop-breaker: unblock lalu block lagi sejenis → triage (limit=2).
        let out = run_json(&unblock, json!({"task_id": solo}));
        assert_eq!(out["status"], "ready");
        let out = run_json(
            &block,
            json!({"task_id": solo, "reason": "masih butuh data", "kind": "needs_input"}),
        );
        assert_eq!(out["status"], "triage");
        assert_eq!(out["block_recurrences"], 2);

        // kind tak dikenal / reason kosong / status tak bisa di-block.
        assert!(rt
            .block_on(block.execute(json!({"task_id": parent, "reason": "x", "kind": "menunggu"})))
            .is_err());
        assert!(rt
            .block_on(block.execute(json!({"task_id": parent, "reason": "   "})))
            .is_err());
        db.complete_task(&parent, Some("ok"), None, None).unwrap();
        assert!(rt
            .block_on(block.execute(json!({"task_id": parent, "reason": "x"})))
            .is_err());
    }

    #[test]
    fn unblock_regates_on_parents() {
        let home = TestHome::new("unblock");
        let db = home.db();
        let block = KanbanBlockHandler;
        let unblock = KanbanUnblockHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        db.move_task(&child, STATUS_READY, None).unwrap(); // simulasi dispatch
        run_json(
            &block,
            json!({"task_id": child, "reason": "butuh masukan", "kind": "needs_input"}),
        );
        // Parent masih terbuka → unblock mendarat di todo.
        let out = run_json(&unblock, json!({"task_id": child}));
        assert_eq!(out["status"], "todo");

        // Selesaikan parent; block lagi dari ready lalu unblock → ready.
        db.complete_task(&parent, Some("ok"), None, None).unwrap();
        db.move_task(&child, STATUS_READY, None).unwrap();
        run_json(
            &block,
            json!({"task_id": child, "reason": "cek ulang", "kind": "transient"}),
        );
        let out = run_json(&unblock, json!({"task_id": child}));
        assert_eq!(out["status"], "ready");
        // block_kind/recurrences dipertahankan melewati unblock (anti-amnesia).
        let t = db.get_task(&child).unwrap().unwrap();
        assert_eq!(t.block_kind.as_deref(), Some("transient"));

        // Unblock task yang tidak blocked → error.
        assert!(rt
            .block_on(unblock.execute(json!({"task_id": child})))
            .is_err());
        // task_id wajib.
        assert!(rt.block_on(unblock.execute(json!({}))).is_err());
    }

    #[test]
    fn schedule_parks_and_unblocks() {
        let home = TestHome::new("schedule");
        let db = home.db();
        let schedule = KanbanScheduleHandler;
        let unblock = KanbanUnblockHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let t = mk(&db, "tugas", vec![]);
        let out = run_json(
            &schedule,
            json!({"task_id": t, "reason": "SCHEDULED_UNTIL=2026-10-22T00:00:00Z"}),
        );
        assert_eq!(out["status"], "scheduled");
        assert!(out["reason"].as_str().unwrap().contains("SCHEDULED_UNTIL"));

        // Scheduled bisa di-unblock kembali ke ready.
        let out = run_json(&unblock, json!({"task_id": t}));
        assert_eq!(out["status"], "ready");

        // Triage tidak bisa di-schedule (di luar todo/ready/running/blocked).
        let t2 = mk(&db, "tugas2", vec![]);
        db.move_task(&t2, STATUS_TRIAGE, None).unwrap();
        assert!(rt
            .block_on(schedule.execute(json!({"task_id": t2})))
            .is_err());
    }

    #[test]
    fn heartbeat_records_liveness() {
        let home = TestHome::new("heartbeat");
        let db = home.db();
        let hb = KanbanHeartbeatHandler;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let t = mk(&db, "panjang", vec![]);
        assert!(db
            .get_task(&t)
            .unwrap()
            .unwrap()
            .last_heartbeat_at
            .is_none());
        let out = run_json(&hb, json!({"task_id": t, "note": "50% jalan"}));
        assert_eq!(out["task_id"], t);
        let task = db.get_task(&t).unwrap().unwrap();
        assert!(task.last_heartbeat_at.is_some());
        let events = db.list_events(&t, 5).unwrap();
        assert_eq!(events[0].kind, "heartbeat");
        assert!(events[0].payload.as_deref().unwrap().contains("50% jalan"));

        // Task fiktif → error.
        assert!(rt
            .block_on(hb.execute(json!({"task_id": "t_tidakada"})))
            .is_err());
    }

    #[test]
    fn task_id_env_fallback() {
        let home = TestHome::new("env-fallback");
        let db = home.db();
        let t = mk(&db, "env task", vec![]);
        std::env::set_var("HERMES_KANBAN_TASK", &t);

        let hb = KanbanHeartbeatHandler;
        let out = run_json(&hb, json!(null));
        assert_eq!(out["task_id"], t);

        let complete = KanbanCompleteHandler;
        let out = run_json(&complete, json!({"summary": "selesai via env"}));
        assert_eq!(out["task_id"], t);
        assert_eq!(out["status"], "done");
    }
}
