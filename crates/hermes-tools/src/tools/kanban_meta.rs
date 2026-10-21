//! Tool kanban: review, komentar, attachment, dan link.
//!
//! Port dari `tools_meta.rs` (batch 7 hermes-rs) ke pola `ToolHandler` fork
//! ini. Ketujuh tool:
//!
//! | Nama | Fungsi |
//! |------|--------|
//! | `kanban_request_review` | serahkan task ke kolom `review` untuk ditinjau |
//! | `kanban_request_changes` | verdict reviewer: kembalikan ke implementer |
//! | `kanban_comment` | tambah komentar ke thread task |
//! | `kanban_attach` | lampirkan file via bytes base64 |
//! | `kanban_attach_url` | lampirkan file via URL (fetch server-side, SSRF guard) |
//! | `kanban_attachments` | daftar attachment sebuah task |
//! | `kanban_link` | tambah edge dependensi parent → child |
//!
//! Beda dengan aslinya: port ini tidak punya machinery
//! dispatcher/claim/run (`expected_run_id`, `claim_lock`, `task_runs`),
//! jadi guard kepemilikan worker (`_worker_guard` di aslinya) disederhanakan
//! menjadi resolusi `task_id` (argumen → env `HERMES_KANBAN_TASK`).
//! Transisi status tetap divalidasi lewat matriks workflow
//! (`BoardDb::move_task`).
//!
//! Karena `ToolHandler::execute` tidak menerima konteks, env dibaca dari
//! environment proses (`std::env::var`): `HERMES_KANBAN_HOME`,
//! `HERMES_KANBAN_BOARD`, `HERMES_KANBAN_TASK`, `HERMES_KANBAN_PROFILE`.

use crate::tools::kanban_store::{
    normalize_board_slug, BoardDb, KanbanStore, KanbanTask, TaskPatch, MAX_ATTACHMENT_BYTES,
};
use crate::tools::ssrf_guard::ensure_url_allowed;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures::StreamExt;
use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helper internal
// ---------------------------------------------------------------------------

/// Resolusi kanban home: `HERMES_KANBAN_HOME` → `<hermes_home>/kanban` →
/// `<cwd>/.hermes/kanban`. (`hermes_config::hermes_home()` sendiri membaca
/// `HERMES_HOME` lalu `~/.hermes`.)
fn kanban_home() -> PathBuf {
    if let Ok(h) = std::env::var("HERMES_KANBAN_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = hermes_config::hermes_home();
    if home.as_os_str().is_empty() {
        return std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".hermes")
            .join("kanban");
    }
    home.join("kanban")
}

fn opt_str<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(|v| v.as_str())
}

fn req_str<'a>(params: &'a Value, name: &str, tool: &str) -> Result<&'a str, ToolError> {
    opt_str(params, name).ok_or_else(|| {
        ToolError::InvalidParams(format!("{tool}: argumen wajib `{name}` bertipe string"))
    })
}

fn string_prop(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn props(pairs: Vec<(&str, Value)>) -> IndexMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn store_for(tool: &str) -> Result<KanbanStore, ToolError> {
    KanbanStore::open(kanban_home()).map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))
}

/// Resolusi board ala `_DESC_BOARD` di skema aslinya: argumen `board` →
/// env `HERMES_KANBAN_BOARD` → board aktif (`current`) → `"default"`.
fn resolve_board(params: &Value, store: &KanbanStore, tool: &str) -> Result<String, ToolError> {
    if let Some(b) = opt_str(params, "board").filter(|s| !s.trim().is_empty()) {
        return normalize_board_slug(b)
            .map_err(|e| ToolError::InvalidParams(format!("{tool}: {e}")));
    }
    if let Ok(b) = std::env::var("HERMES_KANBAN_BOARD") {
        if !b.trim().is_empty() {
            return normalize_board_slug(&b)
                .map_err(|e| ToolError::InvalidParams(format!("{tool}: {e}")));
        }
    }
    let current = store
        .current_board()
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;
    normalize_board_slug(&current).map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))
}

fn created_by() -> String {
    std::env::var("HERMES_KANBAN_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string())
}

fn board_prop() -> Value {
    string_prop(
        "Slug board kanban target. Bila kosong dipakai HERMES_KANBAN_BOARD, \
         lalu board aktif, lalu 'default'.",
    )
}

fn task_id_prop() -> Value {
    string_prop(
        "Id task. Bila kosong dipakai HERMES_KANBAN_TASK dari env \
         (task milik worker yang di-spawn dispatcher).",
    )
}

/// Resolusi `task_id` ala `_require_task_id`/`_worker_guard` di aslinya
/// (tanpa machinery claim dispatcher): argumen → env `HERMES_KANBAN_TASK`.
fn resolve_task_id(params: &Value, tool: &str) -> Result<String, ToolError> {
    if let Some(t) = opt_str(params, "task_id").filter(|s| !s.trim().is_empty()) {
        return Ok(t.to_string());
    }
    std::env::var("HERMES_KANBAN_TASK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ToolError::InvalidParams(format!(
                "{tool}: task_id wajib diisi (atau set HERMES_KANBAN_TASK); \
                 temukan id dengan kanban_list"
            ))
        })
}

fn open_db(params: &Value, tool: &str) -> Result<BoardDb, ToolError> {
    let store = store_for(tool)?;
    let board = resolve_board(params, &store, tool)?;
    store
        .board_db(&board)
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))
}

fn get_task(db: &BoardDb, tool: &str, task_id: &str) -> Result<KanbanTask, ToolError> {
    db.get_task(task_id)
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?
        .ok_or_else(|| ToolError::ExecutionFailed(format!("{tool}: task '{task_id}' tidak ada")))
}

fn opt_string_array(params: &Value, name: &str, tool: &str) -> Result<Vec<String>, ToolError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                    ToolError::InvalidParams(format!("{tool}: `{name}` harus array string"))
                })
            })
            .collect(),
        Some(_) => Err(ToolError::InvalidParams(format!(
            "{tool}: `{name}` harus array string"
        ))),
    }
}

// ---------------------------------------------------------------------------
// kanban_request_review
// ---------------------------------------------------------------------------

/// Serahkan task untuk review: pindahkan ke kolom `review` beserta handoff.
pub struct KanbanRequestReviewHandler;

#[async_trait]
impl ToolHandler for KanbanRequestReviewHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_request_review";
        let task_id = resolve_task_id(&params, tool)?;
        let summary = opt_str(&params, "summary")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "{tool}: summary wajib diisi — jelaskan apa yang diimplementasikan \
                     dan bagaimana diverifikasi agar reviewer punya konteks"
                ))
            })?;
        let reviewer = opt_str(&params, "reviewer")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let metadata: Option<&Value> = match params.get("metadata") {
            None | Some(Value::Null) => None,
            Some(v @ Value::Object(_)) => Some(v),
            Some(_) => {
                return Err(ToolError::InvalidParams(format!(
                    "{tool}: `metadata` harus berupa object"
                )))
            }
        };
        let artifacts = opt_string_array(&params, "artifacts", tool)?;

        let db = open_db(&params, tool)?;
        let task = get_task(&db, tool, &task_id)?;
        if task.status != "running" && task.status != "ready" {
            return Err(ToolError::ExecutionFailed(format!(
                "{tool}: tidak bisa request review untuk {task_id}: task tidak di \
                 running/ready (status: '{}')",
                task.status
            )));
        }

        // Tahap dulu semua artifact SEBELUM ada perubahan state — cerminan
        // ArtifactPreservationError di aslinya: path yang hilang membatalkan
        // seluruh handoff, task tetap di tempat dan bisa dicoba lagi.
        let mut staged: Vec<(String, Vec<u8>)> = Vec::new();
        for path in &artifacts {
            let bytes = tokio::fs::read(path).await.map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "{tool}: tidak bisa mengawetkan artifact '{path}': {e}. Task masih \
                     in-flight (tidak ada perubahan state). Perbaiki path-nya lalu \
                     ulangi handoff yang sama."
                ))
            })?;
            let filename = Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "artifact".to_string());
            staged.push((filename, bytes));
        }

        let implementer = task.assignee.clone();
        if let Some(ref r) = reviewer {
            db.update_task(
                &task_id,
                TaskPatch {
                    assignee: Some(Some(r.clone())),
                    ..Default::default()
                },
            )
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;
        }
        db.move_task(&task_id, "review", Some("review requested"))
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        let mut staged_names = Vec::new();
        for (filename, bytes) in &staged {
            let att = db
                .add_attachment_bytes(&task_id, filename, None, bytes, &created_by())
                .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;
            staged_names.push(att.filename);
        }

        db.record_event(
            &task_id,
            "review_requested",
            Some(
                &json!({
                    "summary": summary,
                    "implementer": implementer,
                    "reviewer": reviewer,
                    "metadata": metadata,
                    "artifacts": staged_names,
                })
                .to_string(),
            ),
        )
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "task_id": task_id,
            "status": "review",
            "implementer": implementer,
            "reviewer": reviewer,
            "artifacts_staged": staged_names,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_request_review",
            "Serahkan task untuk review: implementasi, self-review, dan verifikasi \
             selesai dan kamu ingin reviewer melihat sebelum task ditandai done. \
             Memindahkan task ke kolom 'review'. BUKAN blocker — tidak pernah \
             dihitung ke deteksi unblock-loop, jadi task boleh bolak-balik review \
             tanpa salah dieskalasi ke triage.",
            JsonSchema::object(
                props(vec![
                    ("task_id", task_id_prop()),
                    (
                        "summary",
                        string_prop(
                            "Apa yang diimplementasikan dan bagaimana diverifikasinya, \
                             satu-dua kalimat — ditampilkan ke reviewer. Jangan tempel \
                             seluruh diff; reviewer punya board dan PR.",
                        ),
                    ),
                    (
                        "reviewer",
                        string_prop(
                            "Profil reviewer opsional. Bila diisi, task di-assign ulang \
                             ke profil itu sebelum dispatch review.",
                        ),
                    ),
                    (
                        "metadata",
                        json!({"type": "object",
                               "description": "Fakta handoff terstruktur opsional untuk reviewer, \
                               mis. changed_files, tests_run, commit, atau decisions.",
                               "additionalProperties": true}),
                    ),
                    (
                        "artifacts",
                        json!({"type": "array", "items": {"type": "string"},
                               "description": "Daftar path absolut file deliverable yang \
                               dinamai handoff ini — chart, PDF, spreadsheet, gambar, arsip. \
                               File-file ini disalin ke attachment durable task sebelum \
                               transisi; path yang hilang membatalkan seluruh handoff \
                               (task tetap di tempat, bisa diperbaiki dan dicoba lagi)."}),
                    ),
                    ("board", board_prop()),
                ]),
                vec!["summary".to_string()],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_request_changes
// ---------------------------------------------------------------------------

/// Verdict reviewer: kembalikan task dari `review` ke implementer semula.
pub struct KanbanRequestChangesHandler;

#[async_trait]
impl ToolHandler for KanbanRequestChangesHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_request_changes";
        let task_id = resolve_task_id(&params, tool)?;
        let reason = opt_str(&params, "reason")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "{tool}: reason wajib diisi — jelaskan perubahan yang dibutuhkan"
                ))
            })?;

        let db = open_db(&params, tool)?;
        let task = get_task(&db, tool, &task_id)?;
        if task.status != "review" {
            return Err(ToolError::ExecutionFailed(format!(
                "{tool}: tidak bisa request changes untuk {task_id}: task tidak di \
                 kolom review (status: '{}')",
                task.status
            )));
        }

        // Implementer diambil dari event review_requested terakhir — cerminan
        // provenance `implementer` di aslinya.
        let implementer = db
            .list_events(&task_id, 100)
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?
            .into_iter()
            .find(|e| e.kind == "review_requested")
            .and_then(|e| {
                e.payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<Value>(p).ok())
            })
            .and_then(|v| {
                v.get("implementer")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                ToolError::ExecutionFailed(format!(
                    "{tool}: tidak ada event review_requested dengan provenance \
                     implementer yang valid untuk {task_id} — jalankan \
                     kanban_request_review dulu"
                ))
            })?;
        let reviewer = task.assignee.clone();

        // Kembalikan assignee ke implementer, lalu hitung status pendaratan:
        // `ready` bila semua parent done, selain itu `todo` (parent gating
        // diterapkan ulang, seperti `_landing_status_after_parents` aslinya).
        db.update_task(
            &task_id,
            TaskPatch {
                assignee: Some(Some(implementer.clone())),
                ..Default::default()
            },
        )
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;
        let mut landing = "ready";
        for parent_id in db
            .parents_of(&task_id)
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?
        {
            match db
                .get_task(&parent_id)
                .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?
            {
                Some(p) if p.status == "done" => {}
                _ => {
                    landing = "todo";
                    break;
                }
            }
        }
        db.move_task(&task_id, landing, Some("changes requested"))
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;
        db.record_event(
            &task_id,
            "changes_requested",
            Some(
                &json!({
                    "reason": reason,
                    "implementer": implementer,
                    "reviewer": reviewer,
                    "status": landing,
                })
                .to_string(),
            ),
        )
        .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "task_id": task_id,
            "status": landing,
            "implementer": implementer,
            "reviewer": reviewer,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_request_changes",
            "Verdict reviewer: kembalikan review run yang aktif ke implementer \
             semula dengan daftar perubahan konkret yang wajib dikerjakan. Menutup \
             review run, menerapkan ulang parent dependency gating, dan me-requeue \
             task tanpa memakai akuntansi block-loop. Hanya dipakai dari task yang \
             di-claim dari kolom review; kanban_block hanya untuk blocker eksternal \
             yang asli.",
            JsonSchema::object(
                props(vec![
                    ("task_id", task_id_prop()),
                    (
                        "reason",
                        string_prop(
                            "Perubahan spesifik dan actionable yang harus dilakukan \
                             implementer sebelum meminta review lagi.",
                        ),
                    ),
                    ("board", board_prop()),
                ]),
                vec!["reason".to_string()],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_comment
// ---------------------------------------------------------------------------

/// Tambahkan komentar ke thread sebuah task.
pub struct KanbanCommentHandler;

#[async_trait]
impl ToolHandler for KanbanCommentHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_comment";
        // Seperti aslinya: komentar menulis ke board, jadi panggilan telanjang
        // tanpa task_id tetap error — arahkan ke kanban_list, bukan env var.
        let task_id = opt_str(&params, "task_id")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "{tool}: task_id wajib diisi: komentar bersifat per-task dan sesi \
                     ini tidak punya task default. Temukan id task dengan kanban_list."
                ))
            })?;
        let body = req_str(&params, "body", tool)?;
        // Author berasal dari identitas runtime (profil), TIDAK PERNAH dari
        // argumen — cerminan #19713 di aslinya: komentar disuntik ke system
        // prompt worker berikutnya, jadi override author bisa memalsukan
        // direktif dari nama otoritatif seperti `hermes-system`.
        let author = created_by();

        let db = open_db(&params, tool)?;
        let comment = db
            .add_comment(task_id, &author, body)
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "task_id": task_id,
            "comment_id": comment.id,
            "author": comment.author,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_comment",
            "Tambahkan komentar ke thread sebuah task. Untuk catatan durable yang \
             harus hidup lebih lama dari run ini (pertanyaan untuk worker \
             berikutnya, temuan parsial, rationale). Penalaran ephemeral tidak \
             cocok di sini — pakai respons normal saja.",
            JsonSchema::object(
                props(vec![
                    (
                        "task_id",
                        string_prop(
                            "Id task (wajib; boleh task sendiri atau milik orang \
                             lain — thread komentar bersifat per-task).",
                        ),
                    ),
                    ("body", string_prop("Isi komentar (mendukung Markdown).")),
                    ("board", board_prop()),
                ]),
                vec!["task_id".to_string(), "body".to_string()],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_attach
// ---------------------------------------------------------------------------

/// Lampirkan file ke task dengan bytes inline (base64).
pub struct KanbanAttachHandler;

#[async_trait]
impl ToolHandler for KanbanAttachHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_attach";
        let task_id = resolve_task_id(&params, tool)?;
        let filename = req_str(&params, "filename", tool)?;
        let content_b64 = req_str(&params, "content_base64", tool)?;
        // Ketat seperti `base64.b64decode(validate=True)` di aslinya.
        let data = B64.decode(content_b64).map_err(|e| {
            ToolError::InvalidParams(format!("{tool}: content_base64 bukan base64 valid: {e}"))
        })?;
        let content_type = opt_str(&params, "content_type");

        let db = open_db(&params, tool)?;
        let att = db
            .add_attachment_bytes(&task_id, filename, content_type, &data, &created_by())
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "task_id": task_id,
            "attachment_id": att.id,
            "filename": att.filename,
            "size": att.size,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_attach",
            "Lampirkan file ke task dengan bytes inline (base64). Untuk artefak \
             file asli yang harus bisa diunduh worker berikutnya atau manusia — \
             laporan, gambar, ekspor. File disimpan sebagai attachment sungguhan \
             (bukan tautan komentar) di direktori attachment task, dibatasi 25 MB. \
             Bila hanya punya URL, pakai kanban_attach_url.",
            JsonSchema::object(
                props(vec![
                    ("task_id", task_id_prop()),
                    (
                        "filename",
                        string_prop(
                            "Nama file untuk menyimpannya (mis. 'report.pdf'). \
                             Komponen direktori dibuang; hanya leaf yang dipakai.",
                        ),
                    ),
                    (
                        "content_base64",
                        string_prop("Isi file, base64-encoded. Maks 25 MB setelah decode."),
                    ),
                    (
                        "content_type",
                        string_prop("Tipe MIME opsional (mis. 'application/pdf')."),
                    ),
                    ("board", board_prop()),
                ]),
                vec!["filename".to_string(), "content_base64".to_string()],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_attach_url
// ---------------------------------------------------------------------------

const MAX_ATTACH_URL_REDIRECTS: usize = 5;

/// Fetch `url` lewat http(s) dengan batas `max_bytes` — cerminan
/// `_download_url_with_cap` di aslinya. Setiap hop diperiksa SSRF
/// (`ensure_url_allowed`: skema http(s), tanpa kredensial, bukan
/// privat/internal) dan redirect diikuti manual (maks 5), sehingga URL yang
/// dikontrol model tidak bisa mencapai loopback/private/cloud-metadata.
/// Body dibaca streaming dan batas ukuran dicek saat streaming, jadi tidak
/// ada yang oversize yang di-buffer.
async fn download_url_with_cap(
    url: &str,
    max_bytes: usize,
) -> std::result::Result<(Vec<u8>, Option<String>), String> {
    let client = reqwest::Client::builder()
        .user_agent("hermes-kanban/attach")
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("gagal membangun HTTP client: {e}"))?;
    let mut current = url.to_string();
    for _ in 0..=MAX_ATTACH_URL_REDIRECTS {
        ensure_url_allowed(&current).await?;
        let resp = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("gagal fetch {current}: {e}"))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("redirect tanpa header Location dari {current}"))?;
            let base =
                url::Url::parse(&current).map_err(|e| format!("URL tidak valid {current}: {e}"))?;
            current = base
                .join(location)
                .map_err(|e| format!("gagal resolve redirect: {e}"))?
                .to_string();
            continue;
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("HTTP error dari {current}: {e}"))?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty());
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("gagal membaca body dari {current}: {e}"))?;
            out.extend_from_slice(&chunk);
            if out.len() > max_bytes {
                return Err(format!(
                    "attachment melebihi batas {} MB",
                    max_bytes / (1024 * 1024)
                ));
            }
        }
        return Ok((out, content_type));
    }
    Err(format!("terlalu banyak redirect saat fetch {url}"))
}

/// Decode persen sederhana untuk nama file turunan URL
/// (cerminan `urllib.parse.unquote` di aslinya).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h * 16 + l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Tentukan nama file untuk `kanban_attach_url`: argumen `filename` eksplisit
/// (atau alias pra-skema `title` yang masih dihormati aslinya) → leaf path
/// URL → `"download"`.
fn derive_filename(url: &str, explicit: Option<&str>) -> String {
    if let Some(f) = explicit.filter(|s| !s.trim().is_empty()) {
        return f.trim().to_string();
    }
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.next_back())
                .map(|leaf| percent_decode(leaf).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "download".to_string())
}

/// Lampirkan file ke task via URL — Hermes mengunduhnya server-side.
pub struct KanbanAttachUrlHandler;

#[async_trait]
impl ToolHandler for KanbanAttachUrlHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_attach_url";
        let task_id = resolve_task_id(&params, tool)?;
        let url = req_str(&params, "url", tool)?.trim().to_string();
        // `title` adalah alias pra-skema dari `filename` yang masih dihormati
        // aslinya.
        let explicit = opt_str(&params, "filename").or(opt_str(&params, "title"));
        let filename = derive_filename(&url, explicit);
        let content_type_arg = opt_str(&params, "content_type").map(|s| s.to_string());

        let (data, fetched_ct) = download_url_with_cap(&url, MAX_ATTACHMENT_BYTES)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        let db = open_db(&params, tool)?;
        let ct = content_type_arg.or(fetched_ct);
        let att = db
            .add_attachment_bytes(&task_id, &filename, ct.as_deref(), &data, &created_by())
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "task_id": task_id,
            "attachment_id": att.id,
            "filename": att.filename,
            "size": att.size,
            "source_url": url,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_attach_url",
            "Lampirkan file ke task via URL — Hermes mengunduhnya server-side dan \
             menyimpannya sebagai attachment sungguhan (dibatasi 25 MB). Untuk \
             kasus kamu punya tautan, bukan bytes-nya. Hanya URL http/https yang \
             diterima; target privat/internal ditolak proteksi SSRF.",
            JsonSchema::object(
                props(vec![
                    ("task_id", task_id_prop()),
                    (
                        "url",
                        string_prop("URL http(s) yang di-fetch dan disimpan."),
                    ),
                    (
                        "filename",
                        string_prop(
                            "Nama opsional untuk menyimpannya. Default: komponen \
                             leaf path URL.",
                        ),
                    ),
                    (
                        "content_type",
                        string_prop(
                            "Override tipe MIME opsional. Default: Content-Type \
                             yang dikembalikan server.",
                        ),
                    ),
                    ("board", board_prop()),
                ]),
                vec!["url".to_string()],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_attachments
// ---------------------------------------------------------------------------

/// Daftar file yang dilampirkan ke task (read-only).
pub struct KanbanAttachmentsHandler;

#[async_trait]
impl ToolHandler for KanbanAttachmentsHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_attachments";
        let task_id = resolve_task_id(&params, tool)?;
        let db = open_db(&params, tool)?;
        get_task(&db, tool, &task_id)?;
        let attachments = db
            .list_attachments(&task_id)
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "ok": true,
            "task_id": task_id,
            "attachments": attachments,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_attachments",
            "Daftar file yang dilampirkan ke task: id, filename, content_type, \
             size, siapa pengunggahnya, dan path on-disk absolut yang bisa dibaca. \
             Read-only; tanpa batasan kepemilikan.",
            JsonSchema::object(
                props(vec![("task_id", task_id_prop()), ("board", board_prop())]),
                vec![],
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// kanban_link
// ---------------------------------------------------------------------------

/// Tambah edge dependensi parent → child.
pub struct KanbanLinkHandler;

#[async_trait]
impl ToolHandler for KanbanLinkHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let tool = "kanban_link";
        let parent_id = req_str(&params, "parent_id", tool)?;
        let child_id = req_str(&params, "child_id", tool)?;
        let db = open_db(&params, tool)?;
        db.add_link(parent_id, child_id)
            .map_err(|e| ToolError::ExecutionFailed(format!("{tool}: {e}")))?;

        Ok(json!({
            "parent_id": parent_id,
            "child_id": child_id,
        })
        .to_string())
    }

    fn schema(&self) -> ToolSchema {
        tool_schema(
            "kanban_link",
            "Tambah edge dependensi parent→child setelah kedua task sudah ada. \
             Child tidak akan promosi ke 'ready' sampai semua parent 'done'. \
             Siklus dan self-link ditolak.",
            JsonSchema::object(
                props(vec![
                    ("parent_id", string_prop("Id task parent.")),
                    ("child_id", string_prop("Id task child.")),
                    ("board", board_prop()),
                ]),
                vec!["parent_id".to_string(), "child_id".to_string()],
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::kanban_store::{NewTask, TEST_ENV_LOCK};
    use serde_json::json;

    /// Home kanban terisolasi per tes; guard menahan lock selama tes berjalan.
    /// Lock-nya dibagi dengan modul kanban_lifecycle/kanban_store
    /// (TEST_ENV_LOCK) agar home test tidak bertabrakan antar thread —
    /// lock lokal di sini dulu menyebabkan race antar modul.
    struct TestHome {
        _tmp: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn test_home() -> TestHome {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HERMES_KANBAN_HOME", tmp.path().join("kanban"));
        std::env::remove_var("HERMES_KANBAN_BOARD");
        std::env::remove_var("HERMES_KANBAN_TASK");
        std::env::remove_var("HERMES_KANBAN_PROFILE");
        TestHome {
            _tmp: tmp,
            _guard: guard,
        }
    }

    fn store() -> KanbanStore {
        KanbanStore::open(kanban_home()).unwrap()
    }

    fn db() -> BoardDb {
        store().board_db("default").unwrap()
    }

    fn mk_task(title: &str, assignee: &str) -> String {
        db().create_task(NewTask {
            title: title.to_string(),
            assignee: assignee.to_string(),
            ..Default::default()
        })
        .unwrap()
        .id
    }

    fn task_status(id: &str) -> String {
        db().get_task(id).unwrap().unwrap().status
    }

    async fn run(handler: &impl ToolHandler, params: Value) -> Value {
        let out = handler.execute(params).await.expect("tool harus sukses");
        serde_json::from_str(&out).unwrap_or(json!({"raw": out}))
    }

    #[test]
    fn schema_names_required_and_props() {
        let _home = test_home();
        let cases: Vec<(&dyn ToolHandler, &str, Vec<&str>)> = vec![
            (
                &KanbanRequestReviewHandler,
                "kanban_request_review",
                vec!["summary"],
            ),
            (
                &KanbanRequestChangesHandler,
                "kanban_request_changes",
                vec!["reason"],
            ),
            (
                &KanbanCommentHandler,
                "kanban_comment",
                vec!["task_id", "body"],
            ),
            (
                &KanbanAttachHandler,
                "kanban_attach",
                vec!["filename", "content_base64"],
            ),
            (&KanbanAttachUrlHandler, "kanban_attach_url", vec!["url"]),
            (&KanbanAttachmentsHandler, "kanban_attachments", vec![]),
            (
                &KanbanLinkHandler,
                "kanban_link",
                vec!["parent_id", "child_id"],
            ),
        ];
        for (h, name, required) in cases {
            let s = h.schema();
            assert_eq!(s.name, name);
            assert!(!s.description.is_empty(), "{name} butuh deskripsi");
            let req = s.parameters.required.clone().unwrap_or_default();
            for r in required {
                assert!(req.contains(&r.to_string()), "{name} butuh required `{r}`");
            }
            let props_map = s.parameters.properties.clone().unwrap_or_default();
            assert!(
                props_map.contains_key("board") || name == "kanban_link",
                "{name} seharusnya punya prop `board`"
            );
        }
        // kanban_link juga punya board (dicek eksplisit).
        assert!(KanbanLinkHandler
            .schema()
            .parameters
            .properties
            .unwrap()
            .contains_key("board"));
    }

    #[tokio::test]
    async fn request_review_happy_path() {
        let _home = test_home();
        let tid = mk_task("implementasi x", "dev-a");

        let out = run(
            &KanbanRequestReviewHandler,
            json!({"task_id": tid, "summary": "menambah endpoint /x, diuji 12 tes",
                   "reviewer": "reviewer-b",
                   "metadata": {"tests_run": 12, "commit": "abc123"}}),
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["status"], "review");
        assert_eq!(out["implementer"], "dev-a");
        assert_eq!(out["reviewer"], "reviewer-b");
        assert_eq!(task_status(&tid), "review");

        // Event review_requested tercatat dengan provenance.
        let ev = db()
            .list_events(&tid, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "review_requested")
            .expect("event review_requested");
        let payload: Value = serde_json::from_str(ev.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["summary"], "menambah endpoint /x, diuji 12 tes");
        assert_eq!(payload["implementer"], "dev-a");
        assert_eq!(payload["reviewer"], "reviewer-b");
        assert_eq!(payload["metadata"]["tests_run"], 12);
        // Assignee dialihkan ke reviewer.
        assert_eq!(
            db().get_task(&tid).unwrap().unwrap().assignee.as_deref(),
            Some("reviewer-b")
        );
    }

    #[tokio::test]
    async fn request_review_validates() {
        let _home = test_home();
        let tid = mk_task("implementasi y", "dev-a");

        // summary wajib.
        assert!(KanbanRequestReviewHandler
            .execute(json!({"task_id": tid}))
            .await
            .is_err());
        // metadata harus object.
        assert!(KanbanRequestReviewHandler
            .execute(json!({"task_id": tid, "summary": "s", "metadata": "bukan-object"}))
            .await
            .is_err());
        // artifacts harus array string.
        assert!(KanbanRequestReviewHandler
            .execute(json!({"task_id": tid, "summary": "s", "artifacts": [42]}))
            .await
            .is_err());
        // task fiktif.
        assert!(KanbanRequestReviewHandler
            .execute(json!({"task_id": "t_tidakada", "summary": "s"}))
            .await
            .is_err());

        // Task di `todo` (bukan running/ready) ditolak.
        db().move_task(&tid, "todo", None).unwrap();
        assert!(KanbanRequestReviewHandler
            .execute(json!({"task_id": tid, "summary": "s"}))
            .await
            .is_err());
        assert_eq!(task_status(&tid), "todo");
    }

    #[tokio::test]
    async fn request_review_stages_artifacts() {
        let _home = test_home();
        let tid = mk_task("laporan", "dev-a");

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("laporan.pdf");
        std::fs::write(&f1, b"%PDF-1.4 isi laporan").unwrap();

        let out = run(
            &KanbanRequestReviewHandler,
            json!({"task_id": tid, "summary": "laporan selesai",
                   "artifacts": [f1.to_string_lossy()]}),
        )
        .await;
        assert_eq!(out["artifacts_staged"].as_array().unwrap().len(), 1);
        assert_eq!(out["artifacts_staged"][0], "laporan.pdf");

        let atts = db().list_attachments(&tid).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "laporan.pdf");
        assert_eq!(
            std::fs::read(&atts[0].stored_path).unwrap(),
            b"%PDF-1.4 isi laporan"
        );

        // Path hilang → seluruh handoff dibatalkan, task tetap di tempat.
        let tid2 = mk_task("laporan 2", "dev-a");
        let err = KanbanRequestReviewHandler
            .execute(json!({"task_id": tid2, "summary": "s",
                            "artifacts": ["/tidak/ada/file.pdf"]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tidak bisa mengawetkan artifact"));
        assert_eq!(task_status(&tid2), "running");
        assert!(db().list_attachments(&tid2).unwrap().is_empty());
        assert!(db()
            .list_events(&tid2, 10)
            .unwrap()
            .iter()
            .all(|e| e.kind != "review_requested"));
    }

    #[tokio::test]
    async fn request_changes_roundtrip() {
        let _home = test_home();
        let tid = mk_task("fitur z", "dev-a");

        run(
            &KanbanRequestReviewHandler,
            json!({"task_id": tid, "summary": "fitur z selesai", "reviewer": "rev-b"}),
        )
        .await;
        let out = run(
            &KanbanRequestChangesHandler,
            json!({"task_id": tid, "reason": "tambah tes untuk edge case kosong"}),
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["implementer"], "dev-a");
        assert_eq!(out["reviewer"], "rev-b");
        // Tanpa parent → landing `ready`.
        assert_eq!(out["status"], "ready");
        assert_eq!(task_status(&tid), "ready");

        // Assignee dikembalikan ke implementer.
        assert_eq!(
            db().get_task(&tid).unwrap().unwrap().assignee.as_deref(),
            Some("dev-a")
        );
        // Event changes_requested tercatat.
        assert!(db()
            .list_events(&tid, 10)
            .unwrap()
            .iter()
            .any(|e| e.kind == "changes_requested"));
    }

    #[tokio::test]
    async fn request_changes_landing_todo_when_parent_open() {
        let _home = test_home();
        let parent = mk_task("fondasi", "dev-a");
        let child = mk_task("lantai 2", "dev-b");
        db().add_link(&parent, &child).unwrap();

        run(
            &KanbanRequestReviewHandler,
            json!({"task_id": child, "summary": "lantai 2 selesai"}),
        )
        .await;
        let out = run(
            &KanbanRequestChangesHandler,
            json!({"task_id": child, "reason": "perbaiki tangga"}),
        )
        .await;
        // Parent belum done → landing `todo` (gating diterapkan ulang).
        assert_eq!(out["status"], "todo");
        assert_eq!(task_status(&child), "todo");
    }

    #[tokio::test]
    async fn request_changes_validates() {
        let _home = test_home();
        let tid = mk_task("tugas", "dev-a");

        // reason wajib.
        assert!(KanbanRequestChangesHandler
            .execute(json!({"task_id": tid}))
            .await
            .is_err());
        // Hanya dari kolom review.
        assert!(KanbanRequestChangesHandler
            .execute(json!({"task_id": tid, "reason": "ubah"}))
            .await
            .is_err());

        // Di review TANPA event review_requested → provenance hilang → error.
        db().move_task(&tid, "review", None).unwrap();
        assert!(KanbanRequestChangesHandler
            .execute(json!({"task_id": tid, "reason": "ubah"}))
            .await
            .is_err());
        assert_eq!(task_status(&tid), "review");
    }

    #[tokio::test]
    async fn comment_uses_profile_author_and_requires_task() {
        let _home = test_home();
        std::env::set_var("HERMES_KANBAN_PROFILE", "peninjau");
        let tid = mk_task("tugas komentar", "dev-a");

        let out = run(
            &KanbanCommentHandler,
            json!({"task_id": tid, "body": "catatan **penting**"}),
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["author"], "peninjau");

        let comments = db().list_comments(&tid).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "peninjau");
        assert_eq!(comments[0].body, "catatan **penting**");

        // Tanpa task_id → error (tidak ada default env untuk komentar).
        assert!(KanbanCommentHandler
            .execute(json!({"body": "x"}))
            .await
            .is_err());
        // body kosong → error.
        assert!(KanbanCommentHandler
            .execute(json!({"task_id": tid, "body": "   "}))
            .await
            .is_err());
        // task fiktif → error.
        assert!(KanbanCommentHandler
            .execute(json!({"task_id": "t_tidakada", "body": "x"}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn attach_stores_bytes_with_strict_base64() {
        let _home = test_home();
        let tid = mk_task("tugas lampiran", "dev-a");

        let out = run(
            &KanbanAttachHandler,
            json!({"task_id": tid, "filename": "../laporan.pdf",
                   "content_base64": B64.encode(b"isi-pdf"),
                   "content_type": "application/pdf"}),
        )
        .await;
        assert_eq!(out["task_id"], tid);
        // Komponen direktori dibuang.
        assert_eq!(out["filename"], "laporan.pdf");
        assert_eq!(out["size"], 7);

        let atts = db().list_attachments(&tid).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].content_type.as_deref(), Some("application/pdf"));
        assert_eq!(std::fs::read(&atts[0].stored_path).unwrap(), b"isi-pdf");

        // base64 tidak valid → error (ketat seperti validate=True).
        assert!(KanbanAttachHandler
            .execute(json!({"task_id": tid, "filename": "x.bin",
                            "content_base64": "!!!bukan-base64!!!"}))
            .await
            .is_err());
        // task fiktif → error.
        assert!(KanbanAttachHandler
            .execute(json!({"task_id": "t_tidakada", "filename": "x",
                            "content_base64": B64.encode(b"x")}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn attach_url_rejects_unsafe_and_derives_names() {
        let _home = test_home();
        let tid = mk_task("tugas url", "dev-a");

        // Skema non-http ditolak — tanpa jaringan tersentuh.
        let err = KanbanAttachUrlHandler
            .execute(json!({"task_id": tid, "url": "ftp://example.com/f.bin"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http(s)"), "{err}");
        // Loopback ditolak proteksi SSRF — tanpa jaringan tersentuh
        // (IP literal tidak memicu DNS).
        let err = KanbanAttachUrlHandler
            .execute(json!({"task_id": tid, "url": "http://127.0.0.1/f.bin"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");
        // Cloud metadata IP juga ditolak.
        let err = KanbanAttachUrlHandler
            .execute(json!({"task_id": tid, "url": "http://169.254.169.254/x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");
        // URL berkredensial ditolak.
        let err = KanbanAttachUrlHandler
            .execute(json!({"task_id": tid, "url": "https://user:pw@93.184.216.34/f"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");

        // Derivasi nama file (murni, tanpa jaringan).
        assert_eq!(
            derive_filename("https://example.com/dir/laporan%20q3.pdf?x=1", None),
            "laporan q3.pdf"
        );
        assert_eq!(
            derive_filename("https://example.com/dir/", None),
            "download"
        );
        assert_eq!(
            derive_filename("https://example.com/a.pdf", Some("  custom.pdf ")),
            "custom.pdf"
        );
        assert_eq!(derive_filename("https://example.com/a.pdf", None), "a.pdf");
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[tokio::test]
    async fn attachments_lists_with_fields() {
        let _home = test_home();
        let tid = mk_task("tugas daftar", "dev-a");

        // Kosong → list kosong.
        let out = run(&KanbanAttachmentsHandler, json!({"task_id": tid})).await;
        assert_eq!(out["ok"], true);
        assert_eq!(out["attachments"].as_array().unwrap().len(), 0);

        run(
            &KanbanAttachHandler,
            json!({"task_id": tid, "filename": "a.txt",
                   "content_base64": B64.encode(b"abc")}),
        )
        .await;
        let out = run(&KanbanAttachmentsHandler, json!({"task_id": tid})).await;
        let atts = out["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["filename"], "a.txt");
        assert_eq!(atts[0]["size"], 3);
        assert!(atts[0]["stored_path"].as_str().unwrap().contains(&tid));
        assert_eq!(atts[0]["uploaded_by"], "agent");

        // task fiktif → error.
        assert!(KanbanAttachmentsHandler
            .execute(json!({"task_id": "t_tidakada"}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn link_adds_edge_and_rejects_bad_links() {
        let _home = test_home();
        let a = mk_task("task a", "dev-a");
        let b = mk_task("task b", "dev-b");

        let out = run(&KanbanLinkHandler, json!({"parent_id": a, "child_id": b})).await;
        assert_eq!(out["parent_id"], a);
        assert_eq!(out["child_id"], b);
        assert_eq!(db().parents_of(&b).unwrap(), vec![a.clone()]);

        // Self-link ditolak.
        assert!(KanbanLinkHandler
            .execute(json!({"parent_id": a, "child_id": a}))
            .await
            .is_err());
        // Siklus ditolak.
        assert!(KanbanLinkHandler
            .execute(json!({"parent_id": b, "child_id": a}))
            .await
            .is_err());
        // Task fiktif ditolak.
        assert!(KanbanLinkHandler
            .execute(json!({"parent_id": a, "child_id": "t_tidakada"}))
            .await
            .is_err());
        // Argumen wajib.
        assert!(KanbanLinkHandler
            .execute(json!({"parent_id": a}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn task_id_defaults_to_env_but_not_for_comment() {
        let _home = test_home();
        let tid = mk_task("tugas env", "dev-a");
        std::env::set_var("HERMES_KANBAN_TASK", &tid);

        // Tool selain komentar memakai env fallback.
        let out = run(&KanbanAttachmentsHandler, json!({})).await;
        assert_eq!(out["task_id"], tid);
        // kanban_comment sengaja TIDAK pakai default env.
        assert!(KanbanCommentHandler
            .execute(json!({"body": "x"}))
            .await
            .is_err());
    }
}
