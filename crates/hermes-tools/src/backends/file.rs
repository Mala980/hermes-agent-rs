//! Real file tool backends: patch (fuzzy match) and search (regex/glob).

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use crate::tools::file::{PatchBackend, SearchBackend};
use hermes_core::ToolError;

// ---------------------------------------------------------------------------
// LocalPatchBackend
// ---------------------------------------------------------------------------

/// Real file patch backend with reference `patch` semantics.
///
/// Matching: exact search first (must match exactly once), then a
/// whitespace-normalized fuzzy fallback (the whitespace-separated tokens of
/// the needle joined by `\s+`; must also match exactly once). Zero or
/// multiple matches is an error and the file is left untouched. A successful
/// patch reports a unified diff (context radius 3) of the change.
pub struct LocalPatchBackend;

impl LocalPatchBackend {
    pub fn new() -> Self {
        Self
    }

    /// Whitespace-normalized fuzzy replacement semantics, mirroring the reference
    /// `patch`/`edit_file` (`apply_replace`): exact match first (must be unique),
    /// then a fuzzy fallback where the whitespace-separated tokens of `old` may be
    /// separated by any whitespace run (tokens joined by `\s+`; must be unique).
    /// Returns the new content and whether the match was `"exact"` or `"fuzzy"`.
    fn apply_single_replace(
        content: &str,
        old: &str,
        new: &str,
    ) -> Result<(String, &'static str), String> {
        let exact = content.matches(old).count();
        if exact == 1 {
            return Ok((content.replacen(old, new, 1), "exact"));
        }
        if exact > 1 {
            return Err(format!(
                "ambiguous: old_string matched {exact} times exactly; \
             include surrounding context lines to make it unique"
            ));
        }
        // No exact match: fuzzy fallback — tokens of old_string may be separated
        // by any run of whitespace (spaces, tabs, newlines).
        let tokens: Vec<&str> = old.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(
                "no match found: old_string not found in file, neither exactly \
                    nor with whitespace-normalized fuzzy matching"
                    .to_string(),
            );
        }
        let pattern = tokens
            .iter()
            .map(|t| regex::escape(t))
            .collect::<Vec<_>>()
            .join(r"\s+");
        let re = Regex::new(&pattern).map_err(|e| format!("invalid fuzzy pattern: {e}"))?;
        let spans: Vec<_> = re.find_iter(content).collect();
        match spans.len() {
            0 => Err(
                "no match found: old_string not found in file, neither exactly \
                  nor with whitespace-normalized fuzzy matching"
                    .to_string(),
            ),
            1 => {
                let m = spans[0];
                let mut out = String::with_capacity(content.len() + new.len());
                out.push_str(&content[..m.start()]);
                out.push_str(new);
                out.push_str(&content[m.end()..]);
                Ok((out, "fuzzy"))
            }
            n => Err(format!(
                "ambiguous: old_string matched {n} times with whitespace-normalized \
             fuzzy matching; include surrounding context lines to make it unique"
            )),
        }
    }

    /// Render a unified diff (`---`/`+++` + `@@` hunks) between the old and new
    /// file contents, with context radius 3. A patch always replaces a single
    /// contiguous span, so exactly one hunk is emitted.
    fn unified_diff(old: &str, new: &str, name: &str) -> String {
        const RADIUS: usize = 3;
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();

        let mut prefix = 0;
        while prefix < old_lines.len()
            && prefix < new_lines.len()
            && old_lines[prefix] == new_lines[prefix]
        {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_lines.len() - prefix
            && suffix < new_lines.len() - prefix
            && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
        {
            suffix += 1;
        }

        let ctx_before = prefix.min(RADIUS);
        let ctx_after = suffix.min(RADIUS);
        let old_start = prefix - ctx_before; // 0-indexed
        let old_changed = old_lines.len() - prefix - suffix;
        let new_changed = new_lines.len() - prefix - suffix;

        let mut out = String::new();
        out.push_str(&format!("--- a/{name}\n+++ b/{name}\n"));
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start + 1,
            ctx_before + old_changed + ctx_after,
            old_start + 1,
            ctx_before + new_changed + ctx_after,
        ));
        for line in &old_lines[old_start..prefix] {
            out.push_str(&format!(" {line}\n"));
        }
        for line in &old_lines[prefix..old_lines.len() - suffix] {
            out.push_str(&format!("-{line}\n"));
        }
        for line in &new_lines[prefix..new_lines.len() - suffix] {
            out.push_str(&format!("+{line}\n"));
        }
        let after = old_lines.len() - suffix;
        for line in &old_lines[after..after + ctx_after] {
            out.push_str(&format!(" {line}\n"));
        }
        out
    }

    /// Short preview of a needle for error messages.
    fn truncate_preview(s: &str, max_chars: usize) -> String {
        if s.chars().count() <= max_chars {
            s.to_string()
        } else {
            s.chars().take(max_chars).collect()
        }
    }
}

impl Default for LocalPatchBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PatchBackend for LocalPatchBackend {
    async fn patch_file(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String, ToolError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to read '{}': {}", path, e)))?;

        if old_string.is_empty() {
            // Empty old_string means create new file or append
            let new_content = if content.is_empty() {
                new_string.to_string()
            } else {
                format!("{}\n{}", content, new_string)
            };
            tokio::fs::write(path, &new_content).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to write '{}': {}", path, e))
            })?;
            return Ok(json!({"status": "ok", "message": "Content appended"}).to_string());
        }

        if old_string == new_string {
            return Err(ToolError::InvalidParams(
                "new_string must differ from old_string (use empty string to delete)".to_string(),
            ));
        }

        let display_name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);

        if replace_all {
            // Replace every exact occurrence (explicit opt-in; uniqueness is
            // not required in this mode).
            let count = content.matches(old_string).count();
            if count == 0 {
                return Err(ToolError::ExecutionFailed(format!(
                    "Could not find '{}' in '{}'",
                    Self::truncate_preview(old_string, 100),
                    path
                )));
            }
            let new_content = content.replace(old_string, new_string);
            tokio::fs::write(path, &new_content).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to write '{}': {}", path, e))
            })?;
            return Ok(json!({
                "status": "ok",
                "replacements": count,
                "match": "exact",
                "diff": Self::unified_diff(&content, &new_content, display_name),
            })
            .to_string());
        }

        // Single replacement: old_string must match EXACTLY once (reference
        // semantics). Exact search first, then the whitespace-normalized fuzzy
        // fallback; zero or multiple matches is an error and the file is
        // left untouched.
        let (new_content, kind) = Self::apply_single_replace(&content, old_string, new_string)
            .map_err(ToolError::ExecutionFailed)?;

        tokio::fs::write(path, &new_content).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to write '{}': {}", path, e))
        })?;

        Ok(json!({
            "status": "ok",
            "replacements": 1,
            "match": kind,
            "diff": Self::unified_diff(&content, &new_content, display_name),
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// LocalSearchBackend
// ---------------------------------------------------------------------------

/// Real file search backend using regex for content and glob for filenames.
pub struct LocalSearchBackend;

impl LocalSearchBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalSearchBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for LocalSearchBackend {
    async fn search_content(
        &self,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<String, ToolError> {
        let re = Regex::new(pattern)
            .map_err(|e| ToolError::InvalidParams(format!("Invalid regex pattern: {}", e)))?;

        let max = max_results.unwrap_or(50);
        let mut results: Vec<Value> = Vec::new();

        let path = std::path::Path::new(path);
        if !path.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "Path '{}' does not exist",
                path.display()
            )));
        }

        Self::search_dir_content(&re, path, file_glob, max, &mut results).await;

        Ok(json!({
            "matches": results,
            "total": results.len(),
            "pattern": pattern,
        })
        .to_string())
    }

    async fn search_files(&self, pattern: &str, path: &str) -> Result<String, ToolError> {
        let mut results: Vec<Value> = Vec::new();
        let base = std::path::Path::new(path);

        if !base.exists() {
            return Err(ToolError::ExecutionFailed(format!(
                "Path '{}' does not exist",
                base.display()
            )));
        }

        Self::search_dir_names(pattern, base, &mut results).await;

        Ok(json!({
            "files": results,
            "total": results.len(),
            "pattern": pattern,
        })
        .to_string())
    }
}

const MAX_SEARCH_DEPTH: u32 = 12;

impl LocalSearchBackend {
    async fn search_dir_content(
        re: &Regex,
        dir: &std::path::Path,
        file_glob: Option<&str>,
        max: usize,
        results: &mut Vec<Value>,
    ) {
        Self::search_dir_content_depth(re, dir, file_glob, max, results, 0).await;
    }

    async fn search_dir_content_depth(
        re: &Regex,
        dir: &std::path::Path,
        file_glob: Option<&str>,
        max: usize,
        results: &mut Vec<Value>,
        depth: u32,
    ) {
        if results.len() >= max || depth >= MAX_SEARCH_DEPTH {
            return;
        }

        let entries = match tokio::fs::read_dir(dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut entries = entries;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if results.len() >= max {
                break;
            }

            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "__pycache__"
            {
                continue;
            }

            if path.is_dir() {
                Box::pin(Self::search_dir_content_depth(
                    re,
                    &path,
                    file_glob,
                    max,
                    results,
                    depth + 1,
                ))
                .await;
            } else if path.is_file() {
                // Check glob filter
                if let Some(glob) = file_glob {
                    if !Self::matches_glob(name, glob) {
                        continue;
                    }
                }

                // Try to read as text
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    for (line_num, line) in content.lines().enumerate() {
                        if results.len() >= max {
                            break;
                        }
                        if re.is_match(line) {
                            results.push(json!({
                                "file": path.display().to_string(),
                                "line": line_num + 1,
                                "content": line.chars().take(500).collect::<String>(),
                            }));
                        }
                    }
                }
            }
        }
    }

    async fn search_dir_names(pattern: &str, dir: &std::path::Path, results: &mut Vec<Value>) {
        Self::search_dir_names_depth(pattern, dir, results, 0).await;
    }

    async fn search_dir_names_depth(
        pattern: &str,
        dir: &std::path::Path,
        results: &mut Vec<Value>,
        depth: u32,
    ) {
        if depth >= MAX_SEARCH_DEPTH {
            return;
        }

        let entries = match tokio::fs::read_dir(dir).await {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut entries = entries;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }

            if Self::matches_glob(name, pattern) {
                results.push(json!({
                    "path": path.display().to_string(),
                    "name": name,
                    "is_dir": path.is_dir(),
                }));
            }

            if path.is_dir() {
                Box::pin(Self::search_dir_names_depth(
                    pattern,
                    &path,
                    results,
                    depth + 1,
                ))
                .await;
            }
        }
    }

    fn matches_glob(name: &str, pattern: &str) -> bool {
        // Simple glob matching: * matches any sequence, ? matches single char
        let re_pattern = pattern
            .replace('.', "\\.")
            .replace('*', ".*")
            .replace('?', ".");
        Regex::new(&format!("^{}$", re_pattern))
            .map(|re| re.is_match(name))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    fn patch_ok(path: &str, old: &str, new: &str, replace_all: bool) -> serde_json::Value {
        let backend = LocalPatchBackend::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt
            .block_on(backend.patch_file(path, old, new, replace_all))
            .expect("patch_file failed");
        serde_json::from_str(&out).expect("patch result must be JSON")
    }

    fn patch_err(path: &str, old: &str, new: &str) -> String {
        let backend = LocalPatchBackend::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(backend.patch_file(path, old, new, false))
            .expect_err("patch_file should fail")
            .to_string()
    }

    #[test]
    fn patch_exact_unique_replaces_and_reports_diff() {
        let dir = tempfile::tempdir().unwrap();
        let original = "line1\nline2\nhello world\nline4\nline5\nline6\nline7\n";
        let path = write_tmp(&dir, "a.txt", original);

        let v = patch_ok(&path, "hello world", "halo dunia", false);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["replacements"], 1);
        assert_eq!(v["match"], "exact");

        let diff = v["diff"].as_str().expect("diff must be a string");
        assert!(diff.contains("---"), "diff:\n{diff}");
        assert!(diff.contains("+++"), "diff:\n{diff}");
        assert!(diff.contains("@@"), "diff:\n{diff}");
        assert!(diff.contains("-hello world"), "diff:\n{diff}");
        assert!(diff.contains("+halo dunia"), "diff:\n{diff}");
        // Radius-3 context: 3 unchanged lines on each side of the hunk.
        assert!(diff.contains(" line2"), "diff:\n{diff}");
        assert!(diff.contains(" line5"), "diff:\n{diff}");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nline2\nhalo dunia\nline4\nline5\nline6\nline7\n"
        );
    }

    #[test]
    fn patch_ambiguous_exact_errors_with_count_and_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = "foo\nbar\nfoo\nbaz\nfoo\n";
        let path = write_tmp(&dir, "a.txt", original);

        let err = patch_err(&path, "foo", "qux");
        assert!(err.contains('3'), "error must state the match count: {err}");
        assert!(err.contains("ambiguous"), "error must say ambiguous: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn patch_zero_match_errors_and_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = "line1\nline2\n";
        let path = write_tmp(&dir, "a.txt", original);

        let err = patch_err(&path, "nothing like this", "x");
        assert!(err.contains("no match"), "error: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn patch_fuzzy_whitespace_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        // Double space in the file, single space in old_string.
        let path = write_tmp(&dir, "a.txt", "fn main() {\n    let x =  1;\n}\n");

        let v = patch_ok(&path, "let x = 1;", "let x = 2;", false);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["match"], "fuzzy");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );
    }

    #[test]
    fn patch_fuzzy_ambiguous_errors_with_count() {
        let dir = tempfile::tempdir().unwrap();
        // Two whitespace variants of the same tokens: both match fuzzily.
        let path = write_tmp(&dir, "a.txt", "let  x = 1;\nlet x =  1;\n");

        let err = patch_err(&path, "let x = 1;", "let x = 2;");
        assert!(err.contains('2'), "error must state the count: {err}");
        assert!(err.contains("ambiguous"), "error: {err}");
        assert!(err.contains("fuzzy"), "error: {err}");
    }

    #[test]
    fn patch_old_equals_new_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tmp(&dir, "a.txt", "same\n");

        let err = patch_err(&path, "same", "same");
        assert!(err.contains("differ"), "error: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "same\n");
    }

    #[test]
    fn patch_replace_all_counts_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tmp(&dir, "a.txt", "foo one\nfoo two\nfoo three\n");

        let v = patch_ok(&path, "foo", "bar", true);
        assert_eq!(v["replacements"], 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "bar one\nbar two\nbar three\n"
        );
    }

    #[test]
    fn patch_replace_all_without_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_tmp(&dir, "a.txt", "nothing here\n");

        let backend = LocalPatchBackend::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(backend.patch_file(&path, "zzz", "y", true))
            .expect_err("should fail")
            .to_string();
        assert!(err.contains("Could not find"), "error: {err}");
    }

    #[test]
    fn unified_diff_uses_radius_three_context() {
        // 10 lines, change line 5 (1-indexed): 3 context lines each side.
        let old: String = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut new_lines: Vec<String> = (1..=10).map(|i| format!("line{i}")).collect();
        new_lines[4] = "CHANGED".to_string();
        let new = new_lines.join("\n");

        let diff = LocalPatchBackend::unified_diff(&old, &new, "f.txt");
        assert!(diff.contains("--- a/f.txt"), "diff:\n{diff}");
        assert!(diff.contains("+++ b/f.txt"), "diff:\n{diff}");
        assert!(diff.contains("@@ -2,7 +2,7 @@"), "diff:\n{diff}");
        assert!(diff.contains("-line5"), "diff:\n{diff}");
        assert!(diff.contains("+CHANGED"), "diff:\n{diff}");
        // Exactly 3 context lines before and after.
        for l in ["line2", "line3", "line4", "line6", "line7", "line8"] {
            assert!(
                diff.contains(&format!(" {l}\n")),
                "missing context {l}:\n{diff}"
            );
        }
        assert!(!diff.contains(" line1\n"), "radius exceeded:\n{diff}");
        assert!(!diff.contains(" line9\n"), "radius exceeded:\n{diff}");
    }

    #[test]
    fn unified_diff_single_line_change() {
        let diff = LocalPatchBackend::unified_diff("a\n", "b\n", "f.txt");
        assert!(diff.contains("-a"), "diff:\n{diff}");
        assert!(diff.contains("+b"), "diff:\n{diff}");
        assert!(diff.contains("@@ -1,1 +1,1 @@"), "diff:\n{diff}");
    }

    #[test]
    fn local_search_backend_content_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().to_string();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n// hello\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "hello again\n").unwrap();

        let backend = LocalSearchBackend::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let out = rt
            .block_on(backend.search_content("hello", &base, None, None))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total"], 2);
        assert!(v["matches"].is_array());

        let out = rt.block_on(backend.search_files("*.rs", &base)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total"], 1);
        assert!(v["files"].is_array());
    }
}
