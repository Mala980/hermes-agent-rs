//! `desktop_project`: create, switch, and list desktop Projects (named workspaces).
//!
//! The reference implementation keeps projects in a per-profile SQLite
//! `projects.db` (`hermes_cli.projects_db`) and lets the GUI gateway re-anchor
//! the live session's workspace through an injected callback.
//!
//! This port stores projects in JSON (`<hermes_home>/projects.json`) instead
//! of SQLite — same semantics, no native dependency:
//! ```json
//! {"active_id": "…", "projects": [{"id": "…", "slug": "…", "name": "…", "primary_path": "…"}]}
//! ```
//!
//! LIMITATIONS vs the Python original:
//! - No SQLite `projects.db`; a JSON file is used instead (per the port spec).
//! - Session-scoped "active project" tracking (the terminal override table's
//!   `cwd_source`) is not available here; `list` reports the profile-global
//!   pointer as the active project. The workspace callback still fires when
//!   both a task id and a path are present.
//! - Concurrent writers can race the read-modify-write cycle (no file locking).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use hermes_core::{tool_schema, JsonSchema, ToolError, ToolHandler, ToolSchema};

/// One named workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectFile {
    #[serde(default)]
    active_id: Option<String>,
    #[serde(default)]
    projects: Vec<Project>,
}

fn projects_file() -> PathBuf {
    hermes_config::hermes_home().join("projects.json")
}

/// Turn a human name into a URL-safe slug: lowercase, runs of non-alphanumerics
/// become a single `-`, trimmed of leading/trailing dashes.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Resolve a token against id / slug / name (exact first, then case-insensitive
/// slug / name), mirroring `_resolve` in the reference implementation.
pub fn resolve_token<'a>(projects: &'a [Project], token: &str) -> Option<&'a Project> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    for p in projects {
        if token == p.id || token == p.slug || p.name == token {
            return Some(p);
        }
    }
    let low = token.to_lowercase();
    projects
        .iter()
        .find(|p| p.slug.to_lowercase() == low || p.name.to_lowercase() == low)
}

/// JSON-backed project store.
#[derive(Debug, Clone)]
pub struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> Self {
        Self::new(projects_file())
    }

    fn load(&self) -> Result<ProjectFile, ToolError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "could not parse {}: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProjectFile::default()),
            Err(e) => Err(ToolError::ExecutionFailed(format!(
                "could not read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, file: &ProjectFile) -> Result<(), ToolError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "could not create {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let text = serde_json::to_string_pretty(file).map_err(|e| {
            ToolError::ExecutionFailed(format!("could not serialize projects: {e}"))
        })?;
        std::fs::write(&self.path, text).map_err(|e| {
            ToolError::ExecutionFailed(format!("could not write {}: {e}", self.path.display()))
        })
    }

    /// List all projects plus the active id.
    pub fn list(&self) -> Result<(Option<String>, Vec<Project>), ToolError> {
        let file = self.load()?;
        Ok((file.active_id, file.projects))
    }

    /// Create a project (idempotent: a folder that already belongs to a
    /// project reuses it instead of minting a duplicate). Returns the project.
    pub fn create(&self, name: &str, path: Option<&str>) -> Result<Project, ToolError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ToolError::InvalidParams("name is required".into()));
        }
        let folder = path
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let expanded = shellexpand(s);
                // Canonicalize without requiring existence: absolute + clean.
                let p = PathBuf::from(expanded);
                if p.is_absolute() {
                    p
                } else {
                    std::env::current_dir().unwrap_or_default().join(p)
                }
            })
            .map(|p| p.to_string_lossy().into_owned());

        let mut file = self.load()?;
        if let Some(existing) = folder
            .as_deref()
            .and_then(|f| file.projects.iter().find(|p| p.primary_path.as_deref() == Some(f)))
            .cloned()
        {
            return Ok(existing);
        }
        let mut slug = slugify(name);
        if slug.is_empty() {
            slug = format!("project-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        }
        // De-duplicate slugs.
        let base = slug.clone();
        let mut n = 2;
        while file.projects.iter().any(|p| p.slug == slug) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        let proj = Project {
            id: uuid::Uuid::new_v4().to_string(),
            slug,
            name: name.to_string(),
            primary_path: folder,
        };
        // No live session to move here: switch through the profile-global pointer.
        file.active_id = Some(proj.id.clone());
        file.projects.push(proj.clone());
        self.save(&file)?;
        Ok(proj)
    }

    /// Switch the profile-global pointer to the project matching `token`.
    pub fn switch(&self, token: &str) -> Result<Project, ToolError> {
        let mut file = self.load()?;
        let proj = resolve_token(&file.projects, token)
            .cloned()
            .ok_or_else(|| {
                ToolError::NotFound(format!("no project matching '{token}'"))
            })?;
        file.active_id = Some(proj.id.clone());
        self.save(&file)?;
        Ok(proj)
    }
}

/// Expand a leading `~` to the user home directory.
fn shellexpand(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

/// Callback fired after create/switch when a live session should follow:
/// `(task_id, primary_path, project_name)`.
pub type WorkspaceCallback = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// `desktop_project` tool handler.
pub struct DesktopProjectHandler {
    store: ProjectStore,
    workspace_callback: Option<WorkspaceCallback>,
}

impl DesktopProjectHandler {
    pub fn new(store: ProjectStore, workspace_callback: Option<WorkspaceCallback>) -> Self {
        Self {
            store,
            workspace_callback,
        }
    }
}

#[async_trait]
impl ToolHandler for DesktopProjectHandler {
    async fn execute(&self, params: Value) -> Result<String, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let task_id = params
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let fire = |proj: &Project| {
            if let (Some(cb), Some(tid), Some(path)) =
                (&self.workspace_callback, &task_id, &proj.primary_path)
            {
                cb(tid, path, &proj.name);
            }
        };

        match action {
            "list" => {
                let (active_id, projects) = self.store.list()?;
                Ok(json!({
                    "active_id": active_id,
                    "projects": projects.iter().map(|p| json!({
                        "id": p.id, "slug": p.slug, "name": p.name,
                        "primary_path": p.primary_path,
                        "active": Some(&p.id) == active_id.as_ref(),
                    })).collect::<Vec<_>>(),
                })
                .to_string())
            }
            "create" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let path = params.get("path").and_then(|v| v.as_str());
                let proj = self.store.create(name, path)?;
                fire(&proj);
                Ok(json!({
                    "success": true, "id": proj.id, "slug": proj.slug,
                    "name": proj.name, "primary_path": proj.primary_path,
                })
                .to_string())
            }
            "switch" => {
                let token = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let proj = self.store.switch(token)?;
                fire(&proj);
                Ok(json!({
                    "success": true, "id": proj.id, "slug": proj.slug,
                    "name": proj.name, "primary_path": proj.primary_path,
                })
                .to_string())
            }
            _ => Err(ToolError::InvalidParams(
                "action must be one of: create, switch, list.".into(),
            )),
        }
    }

    fn schema(&self) -> ToolSchema {
        let mut props = IndexMap::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": ["create", "switch", "list"]}),
        );
        props.insert(
            "name".into(),
            json!({"type": "string", "description": "create: human name. switch: name, slug, or id."}),
        );
        props.insert(
            "path".into(),
            json!({"type": "string", "description": "create: repo/folder to anchor to."}),
        );
        tool_schema(
            "desktop_project",
            "Create or switch desktop Projects (named workspaces). create: one and switch this chat into it — pass path to anchor it to a repo/folder. switch: move this chat into an existing project by name/slug/id. list: all projects + which one this chat is in.",
            JsonSchema::object(props, vec!["action".into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Project> {
        vec![
            Project {
                id: "id-1".into(),
                slug: "alpha".into(),
                name: "Alpha".into(),
                primary_path: Some("/tmp/alpha".into()),
            },
            Project {
                id: "id-2".into(),
                slug: "beta-proj".into(),
                name: "Beta Project".into(),
                primary_path: None,
            },
        ]
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("My Cool Project!"), "my-cool-project");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn resolve_exact_id_slug_name() {
        let ps = sample();
        assert_eq!(resolve_token(&ps, "id-1").unwrap().slug, "alpha");
        assert_eq!(resolve_token(&ps, "beta-proj").unwrap().id, "id-2");
        assert_eq!(resolve_token(&ps, "Beta Project").unwrap().id, "id-2");
    }

    #[test]
    fn resolve_case_insensitive_fallback() {
        let ps = sample();
        assert_eq!(resolve_token(&ps, "ALPHA").unwrap().id, "id-1");
        assert_eq!(resolve_token(&ps, "beta project").unwrap().id, "id-2");
    }

    #[test]
    fn resolve_miss_and_blank() {
        let ps = sample();
        assert!(resolve_token(&ps, "nope").is_none());
        assert!(resolve_token(&ps, "  ").is_none());
    }

    #[test]
    fn store_roundtrip_in_tempdir() {
        let dir = std::env::temp_dir().join(format!("hermes-proj-test-{}", uuid::Uuid::new_v4().simple()));
        let store = ProjectStore::new(dir.join("projects.json"));
        let p = store.create("Demo", Some("/tmp/demo")).unwrap();
        assert_eq!(p.slug, "demo");
        let (active, projects) = store.list().unwrap();
        assert_eq!(active.as_deref(), Some(p.id.as_str()));
        assert_eq!(projects.len(), 1);
        // Idempotent create on the same folder reuses the project.
        let p2 = store.create("Demo Again", Some("/tmp/demo")).unwrap();
        assert_eq!(p2.id, p.id);
        let (_, projects) = store.list().unwrap();
        assert_eq!(projects.len(), 1);
        // Switch by slug.
        let p3 = store.switch("demo").unwrap();
        assert_eq!(p3.id, p.id);
        assert!(store.switch("missing").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_name_matches_original() {
        let h = DesktopProjectHandler::new(
            ProjectStore::new(PathBuf::from("/nonexistent")),
            None,
        );
        assert_eq!(h.schema().name, "desktop_project");
    }
}
