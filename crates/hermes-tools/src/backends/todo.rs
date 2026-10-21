//! Real todo backend: JSON file-based task list.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;

use crate::tools::todo::TodoBackend;
use hermes_core::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoItem {
    id: String,
    title: String,
    description: Option<String>,
    status: String,
    priority: String,
    created_at: String,
}

/// File-based todo backend storing tasks as JSON.
pub struct FileTodoBackend {
    file_path: std::path::PathBuf,
    items: Mutex<Vec<TodoItem>>,
}

impl FileTodoBackend {
    pub fn new(file_path: std::path::PathBuf) -> Self {
        let items = if file_path.exists() {
            std::fs::read_to_string(&file_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Self {
            file_path,
            items: Mutex::new(items),
        }
    }

    pub fn default_path() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let path = std::path::Path::new(&home)
            .join(".hermes")
            .join("todos.json");
        Self::new(path)
    }

    fn save(&self, items: &[TodoItem]) -> Result<(), ToolError> {
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to create dir: {}", e)))?;
        }
        let json = serde_json::to_string_pretty(items)
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to serialize todos: {}", e)))?;
        std::fs::write(&self.file_path, json)
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to write todos: {}", e)))?;
        Ok(())
    }
}

#[async_trait]
impl TodoBackend for FileTodoBackend {
    async fn create(
        &self,
        title: &str,
        description: Option<&str>,
        priority: Option<&str>,
    ) -> Result<String, ToolError> {
        let mut items = self
            .items
            .lock()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let item = TodoItem {
            id: id.clone(),
            title: title.to_string(),
            description: description.map(String::from),
            status: "pending".to_string(),
            priority: priority.unwrap_or("medium").to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        items.push(item.clone());
        self.save(&items)?;
        Ok(json!({"status": "ok", "id": id, "title": title}).to_string())
    }

    async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        priority: Option<&str>,
    ) -> Result<String, ToolError> {
        let mut items = self
            .items
            .lock()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let item = items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| ToolError::NotFound(format!("Todo '{}' not found", id)))?;

        if let Some(t) = title {
            item.title = t.to_string();
        }
        if let Some(d) = description {
            item.description = Some(d.to_string());
        }
        if let Some(s) = status {
            item.status = s.to_string();
        }
        if let Some(p) = priority {
            item.priority = p.to_string();
        }

        self.save(&items)?;
        Ok(json!({"status": "ok", "id": id, "action": "updated"}).to_string())
    }

    async fn list(&self, status: Option<&str>) -> Result<String, ToolError> {
        let items = self
            .items
            .lock()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let filtered: Vec<&TodoItem> = match status {
            Some(s) => items.iter().filter(|i| i.status == s).collect(),
            None => items.iter().collect(),
        };
        let result: Vec<serde_json::Value> = filtered
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    "description": i.description,
                    "status": i.status,
                    "priority": i.priority,
                    "created_at": i.created_at,
                })
            })
            .collect();
        Ok(json!({"todos": result, "total": result.len()}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend(name: &str) -> (FileTodoBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("todos_{name}.json"));
        (FileTodoBackend::new(path), dir)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn create_id(backend: &FileTodoBackend, rt: &tokio::runtime::Runtime, title: &str) -> String {
        let out = rt
            .block_on(backend.create(title, None, None))
            .expect("create failed");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    fn list_items(
        backend: &FileTodoBackend,
        rt: &tokio::runtime::Runtime,
    ) -> Vec<serde_json::Value> {
        let out = rt.block_on(backend.list(None)).expect("list failed");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["todos"].as_array().unwrap().clone()
    }

    #[test]
    fn update_merges_by_id_without_duplicates() {
        let (backend, _dir) = test_backend("merge");
        let rt = rt();

        let id = create_id(&backend, &rt, "first task");
        // Update the same todo twice (status toggle + title change).
        rt.block_on(backend.update(&id, None, None, Some("in_progress"), None))
            .expect("update 1 failed");
        rt.block_on(backend.update(
            &id,
            Some("renamed task"),
            None,
            Some("completed"),
            Some("high"),
        ))
        .expect("update 2 failed");

        let items = list_items(&backend, &rt);
        assert_eq!(
            items.len(),
            1,
            "update must not append duplicates: {items:?}"
        );
        assert_eq!(items[0]["id"], id);
        assert_eq!(items[0]["title"], "renamed task");
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(items[0]["priority"], "high");
        // Unset fields are preserved by the merge.
        assert_eq!(items[0]["description"], serde_json::Value::Null);
    }

    #[test]
    fn update_preserves_order_and_other_items() {
        let (backend, _dir) = test_backend("order");
        let rt = rt();

        let id_a = create_id(&backend, &rt, "task A");
        let id_b = create_id(&backend, &rt, "task B");
        // Toggle the first item's status by id.
        rt.block_on(backend.update(&id_a, None, None, Some("completed"), None))
            .expect("update failed");

        let items = list_items(&backend, &rt);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], id_a);
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(items[1]["id"], id_b);
        assert_eq!(items[1]["status"], "pending");
    }

    #[test]
    fn update_unknown_id_errors() {
        let (backend, _dir) = test_backend("unknown");
        let rt = rt();
        let err = rt
            .block_on(backend.update("no-such-id", None, None, Some("completed"), None))
            .expect_err("unknown id must fail")
            .to_string();
        assert!(err.contains("not found"), "unexpected error: {err}");
        assert!(list_items(&backend, &rt).is_empty());
    }

    #[test]
    fn list_status_filter_still_works() {
        let (backend, _dir) = test_backend("filter");
        let rt = rt();
        let id = create_id(&backend, &rt, "task");
        rt.block_on(backend.update(&id, None, None, Some("completed"), None))
            .unwrap();
        create_id(&backend, &rt, "other");

        let out = rt.block_on(backend.list(Some("completed"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total"], 1);
        assert_eq!(v["todos"][0]["id"], id);
    }
}
