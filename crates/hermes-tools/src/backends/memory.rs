//! Real memory backend: read/write MEMORY.md and USER.md in ~/.hermes/

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashSet;

use crate::memory_match::{locate_entry_index, splice_patch_span};
use crate::tools::memory::MemoryBackend;
use hermes_core::ToolError;

const ENTRY_DELIMITER: &str = "\n§\n";
const MEMORY_CHAR_LIMIT: usize = 2200;
const USER_CHAR_LIMIT: usize = 1375;

/// Real memory backend that stores entries in ~/.hermes/memories/MEMORY.md and USER.md.
pub struct FileMemoryBackend {
    hermes_dir: std::path::PathBuf,
}

impl FileMemoryBackend {
    pub fn new() -> Self {
        let home = dirs_home().unwrap_or_else(|| std::path::PathBuf::from("."));
        Self {
            hermes_dir: home.join(".hermes").join("memories"),
        }
    }

    pub fn with_dir(dir: std::path::PathBuf) -> Self {
        Self { hermes_dir: dir }
    }

    fn path_for(&self, target: &str) -> Result<std::path::PathBuf, ToolError> {
        match target {
            "memory" => Ok(self.hermes_dir.join("MEMORY.md")),
            "user" => Ok(self.hermes_dir.join("USER.md")),
            _ => Err(ToolError::InvalidParams(format!(
                "Invalid target '{}'. Use 'memory' or 'user'.",
                target
            ))),
        }
    }

    async fn ensure_dir(&self) -> Result<(), ToolError> {
        tokio::fs::create_dir_all(&self.hermes_dir)
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to create ~/.hermes/memories: {}", e))
            })
    }

    async fn read_file(&self, path: &std::path::Path) -> Result<String, ToolError> {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(ToolError::ExecutionFailed(format!(
                "Failed to read '{}': {}",
                path.display(),
                e
            ))),
        }
    }

    async fn write_file(&self, path: &std::path::Path, content: &str) -> Result<(), ToolError> {
        self.ensure_dir().await?;
        tokio::fs::write(path, content).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to write '{}': {}", path.display(), e))
        })
    }

    fn parse_entries(content: &str) -> Vec<String> {
        if content.trim().is_empty() {
            return Vec::new();
        }
        content
            .split(ENTRY_DELIMITER)
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    fn format_entries(entries: &[String]) -> String {
        entries.join(ENTRY_DELIMITER)
    }

    fn preview(text: &str) -> String {
        if text.chars().count() <= 80 {
            text.to_string()
        } else {
            let head: String = text.chars().take(80).collect();
            format!("{head}...")
        }
    }

    fn success_response(target: &str, entries: &[String], message: &str) -> String {
        let limit = if target == "user" {
            USER_CHAR_LIMIT
        } else {
            MEMORY_CHAR_LIMIT
        };
        let current = entries.join(ENTRY_DELIMITER).chars().count();
        let pct = current
            .checked_mul(100)
            .and_then(|v| v.checked_div(limit))
            .unwrap_or(0)
            .min(100);
        json!({
            "success": true,
            "target": target,
            "message": message,
            "entries": entries,
            "entry_count": entries.len(),
            "usage": format!("{pct}% - {current}/{limit} chars")
        })
        .to_string()
    }

    fn char_limit_for(target: &str) -> usize {
        if target == "user" {
            USER_CHAR_LIMIT
        } else {
            MEMORY_CHAR_LIMIT
        }
    }
}

impl Default for FileMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

#[async_trait]
impl MemoryBackend for FileMemoryBackend {
    async fn add(&self, target: &str, content: &str) -> Result<String, ToolError> {
        let path = self.path_for(target)?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(ToolError::InvalidParams(
                "Content cannot be empty for action='add'.".to_string(),
            ));
        }

        let content = self.read_file(&path).await?;
        let mut entries = Self::parse_entries(&content);

        if entries.iter().any(|e| e == trimmed) {
            return Ok(Self::success_response(
                target,
                &entries,
                "Entry already exists (no duplicate added).",
            ));
        }

        let mut new_entries = entries.clone();
        new_entries.push(trimmed.to_string());
        let new_total = new_entries.join(ENTRY_DELIMITER).chars().count();
        let limit = Self::char_limit_for(target);
        if new_total > limit {
            let current = entries.join(ENTRY_DELIMITER).chars().count();
            return Err(ToolError::ExecutionFailed(format!(
                "Memory at {current}/{limit} chars. Adding this entry ({}) would exceed the limit.",
                trimmed.chars().count()
            )));
        }

        entries = new_entries;
        self.write_file(&path, &Self::format_entries(&entries))
            .await?;
        Ok(Self::success_response(target, &entries, "Entry added."))
    }

    async fn replace(
        &self,
        target: &str,
        old_text: &str,
        new_content: &str,
    ) -> Result<String, ToolError> {
        let path = self.path_for(target)?;
        let old_text = old_text.trim();
        let new_content = new_content.trim();
        if old_text.is_empty() {
            return Err(ToolError::InvalidParams(
                "old_text cannot be empty for action='replace'.".to_string(),
            ));
        }
        if new_content.is_empty() {
            return Err(ToolError::InvalidParams(
                "content cannot be empty for action='replace'.".to_string(),
            ));
        }

        let content = self.read_file(&path).await?;
        let mut entries = Self::parse_entries(&content);

        let matches: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(idx, e)| {
                if e.contains(old_text) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();

        if matches.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "No entry matched '{}'.",
                old_text
            )));
        }

        if matches.len() > 1 {
            let distinct: HashSet<String> = matches.iter().map(|i| entries[*i].clone()).collect();
            if distinct.len() > 1 {
                let previews: Vec<String> = matches
                    .iter()
                    .map(|i| Self::preview(&entries[*i]))
                    .collect();
                return Err(ToolError::ExecutionFailed(format!(
                    "Multiple entries matched '{}'. Be more specific. Matches: {}",
                    old_text,
                    previews.join(" | ")
                )));
            }
        }

        let idx = matches[0];
        let mut candidate = entries.clone();
        candidate[idx] = new_content.to_string();
        let new_total = candidate.join(ENTRY_DELIMITER).chars().count();
        let limit = Self::char_limit_for(target);
        if new_total > limit {
            return Err(ToolError::ExecutionFailed(format!(
                "Replacement would put memory at {new_total}/{limit} chars."
            )));
        }
        entries = candidate;
        self.write_file(&path, &Self::format_entries(&entries))
            .await?;
        Ok(Self::success_response(target, &entries, "Entry replaced."))
    }

    /// Patch one span inside a single entry (parity with the original's
    /// `memory_patch`): typography-tolerant entry lookup via
    /// [`locate_entry_index`], then a single-span replacement via
    /// [`splice_patch_span`]; the rest of the entry is preserved. The patched
    /// store must still fit the target's char limit.
    async fn patch(
        &self,
        target: &str,
        old_text: &str,
        new_content: &str,
    ) -> Result<String, ToolError> {
        let path = self.path_for(target)?;
        let new_content = new_content.trim();
        if new_content.is_empty() {
            return Err(ToolError::InvalidParams(
                "content cannot be empty for action='patch'.".to_string(),
            ));
        }

        let content = self.read_file(&path).await?;
        let mut entries = Self::parse_entries(&content);

        let idx = locate_entry_index(&entries, old_text.trim())?;
        let patched = splice_patch_span(&entries[idx], old_text.trim(), new_content)?;

        let mut candidate = entries.clone();
        candidate[idx] = patched;
        let new_total = candidate.join(ENTRY_DELIMITER).chars().count();
        let limit = Self::char_limit_for(target);
        if new_total > limit {
            return Err(ToolError::ExecutionFailed(format!(
                "Patch would put memory at {new_total}/{limit} chars."
            )));
        }
        entries = candidate;
        self.write_file(&path, &Self::format_entries(&entries))
            .await?;
        Ok(Self::success_response(target, &entries, "Entry patched."))
    }

    async fn remove(&self, target: &str, old_text: &str) -> Result<String, ToolError> {
        let path = self.path_for(target)?;
        let old_text = old_text.trim();
        if old_text.is_empty() {
            return Err(ToolError::InvalidParams(
                "old_text cannot be empty for action='remove'.".to_string(),
            ));
        }

        let content = self.read_file(&path).await?;
        let mut entries = Self::parse_entries(&content);
        let matches: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(idx, e)| {
                if e.contains(old_text) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();

        if matches.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "No entry matched '{}'.",
                old_text
            )));
        }

        if matches.len() > 1 {
            let distinct: HashSet<String> = matches.iter().map(|i| entries[*i].clone()).collect();
            if distinct.len() > 1 {
                let previews: Vec<String> = matches
                    .iter()
                    .map(|i| Self::preview(&entries[*i]))
                    .collect();
                return Err(ToolError::ExecutionFailed(format!(
                    "Multiple entries matched '{}'. Be more specific. Matches: {}",
                    old_text,
                    previews.join(" | ")
                )));
            }
        }

        let idx = matches[0];
        entries.remove(idx);
        self.write_file(&path, &Self::format_entries(&entries))
            .await?;

        Ok(Self::success_response(target, &entries, "Entry removed."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_backend() -> (FileMemoryBackend, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let backend = FileMemoryBackend::with_dir(dir.path().to_path_buf());
        (backend, dir)
    }

    #[tokio::test]
    async fn patch_replaces_single_span_and_keeps_rest() {
        let (b, _dir) = temp_backend();
        b.add("memory", "the sky is blue today").await.unwrap();
        let out = b
            .patch("memory", "blue", "green")
            .await
            .expect("patch should succeed");
        assert!(out.contains("Entry patched."));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].as_str().unwrap(), "the sky is green today");
    }

    #[tokio::test]
    async fn patch_locates_entry_with_folded_typography() {
        // Entry lookup tolerates typography, but the replaced span must be
        // byte-exact in the entry (reference semantics): here the span is.
        let (b, _dir) = temp_backend();
        b.add("memory", "the “quick” brown fox").await.unwrap();
        let out = b
            .patch("memory", "brown fox", "red fox")
            .await
            .expect("byte-exact span patch should succeed");
        assert!(out.contains("the “quick” red fox"));
    }

    #[tokio::test]
    async fn patch_rejects_non_byte_exact_span_as_noop() {
        // Folded lookup finds the entry, but the typographically different
        // span is not byte-exact in it → no-op rejection, like the reference.
        let (b, _dir) = temp_backend();
        b.add("memory", "the “quick” brown fox").await.unwrap();
        let err = b
            .patch("memory", "the \"quick\" brown fox", "x")
            .await
            .expect_err("non-byte-exact span must be rejected");
        assert!(err.to_string().contains("no-op"));
    }

    #[tokio::test]
    async fn patch_rejects_ambiguous_span() {
        let (b, _dir) = temp_backend();
        b.add("memory", "a b a").await.unwrap();
        let err = b
            .patch("memory", "a", "z")
            .await
            .expect_err("ambiguous span must be rejected");
        assert!(err.to_string().contains("occurs 2 times"));
    }

    #[tokio::test]
    async fn patch_rejects_noop() {
        let (b, _dir) = temp_backend();
        b.add("memory", "hello world").await.unwrap();
        let err = b
            .patch("memory", "missing", "zzz")
            .await
            .expect_err("patch with no match must fail");
        assert!(err.to_string().contains("No memory entry matched"));
    }

    #[tokio::test]
    async fn patch_honors_target_store() {
        let (b, _dir) = temp_backend();
        b.add("memory", "agent note").await.unwrap();
        b.add("user", "user pref").await.unwrap();
        let out = b.patch("user", "pref", "habit").await.unwrap();
        assert!(out.contains("\"target\":\"user\""));
        // The memory store is untouched.
        let out2 = b.remove("memory", "agent note").await.unwrap();
        assert!(out2.contains("Entry removed."));
    }
}
