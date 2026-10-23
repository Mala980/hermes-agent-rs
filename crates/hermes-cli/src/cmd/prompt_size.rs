//! `hermes prompt-size` — analyze prompt token sizes.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/prompt_size.py`.
//! The original built a real (offline) inspection agent so the numbers
//! matched what ships on the wire, then measured the system-prompt tiers,
//! the skills index, memory/user blocks and per-toolset schema bytes.
//! This fork measures the same components from the pieces the CLI
//! assembles: configured system prompt/personality, memory files, context
//! files, the skills index on disk, and the tool schemas the tool registry
//! would send (built exactly like the interactive app builds them).

use hermes_config::{hermes_home, load_config};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// CLI args for `hermes prompt-size`.
#[derive(clap::Args, Debug, Clone)]
pub struct PromptSizeArgs {
    /// Platform whose tool surface to measure (accepted for parity; the
    /// tool registry is platform-independent in this fork).
    #[arg(long, default_value = "cli")]
    pub platform: String,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Byte + char size of one text component.
#[derive(Debug, Clone, Default)]
pub struct ComponentSize {
    pub label: String,
    pub chars: usize,
    pub bytes: usize,
}

impl ComponentSize {
    fn of(label: &str, text: &str) -> Self {
        Self {
            label: label.to_string(),
            chars: text.chars().count(),
            bytes: text.len(),
        }
    }
}

/// Per-skill index cost.
#[derive(Debug, Clone)]
pub struct SkillSize {
    pub name: String,
    pub skill_md_bytes: Option<u64>,
    pub index_line_bytes: usize,
}

/// Per-toolset schema-byte breakdown.
#[derive(Debug, Clone)]
pub struct ToolsetSize {
    pub toolset: String,
    pub tool_count: usize,
    pub json_bytes: usize,
}

/// Full prompt-size breakdown.
#[derive(Debug, Default)]
pub struct PromptBreakdown {
    pub platform: String,
    pub model: String,
    pub components: Vec<ComponentSize>,
    pub tools: Vec<(String, usize)>,
    pub toolsets: Vec<ToolsetSize>,
    pub skills: Vec<SkillSize>,
    pub system_prompt_bytes: usize,
    pub tool_schema_bytes: usize,
}

#[allow(dead_code)]
fn read_file_bytes(path: &PathBuf) -> usize {
    std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0)
}

/// Measure the static prompt components assembled from config + home dir.
pub fn measure_components() -> (Vec<ComponentSize>, String) {
    let home = hermes_home();
    let mut components = Vec::new();

    let config = load_config(None).ok();
    let model = config
        .as_ref()
        .and_then(|c| c.model.clone())
        .unwrap_or_default();

    let sys_prompt = config
        .as_ref()
        .and_then(|c| c.system_prompt.clone())
        .unwrap_or_default();
    if !sys_prompt.is_empty() {
        let c = ComponentSize::of("system prompt (config override)", &sys_prompt);
        components.push(c);
    }
    let personality = config
        .as_ref()
        .and_then(|c| c.personality.clone())
        .unwrap_or_default();
    if !personality.is_empty() {
        let text = format!("personality: {personality}");
        let c = ComponentSize::of("personality", &text);
        components.push(c);
    }

    // Memory files: ~/.hermes/memories/MEMORY.md + USER.md.
    for (label, file) in [
        ("memory (MEMORY.md)", "MEMORY.md"),
        ("user profile (USER.md)", "USER.md"),
    ] {
        let path = home.join("memories").join(file);
        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let c = ComponentSize::of(label, &text);
                components.push(c);
            }
        }
    }

    // Global context files: ~/.hermes/context/*.md.
    let context_dir = home.join("context");
    let mut context_bytes = 0usize;
    let mut context_chars = 0usize;
    let mut context_files = 0usize;
    if let Ok(rd) = std::fs::read_dir(&context_dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    context_files += 1;
                    context_chars += text.chars().count();
                    context_bytes += text.len();
                }
            }
        }
    }
    if context_files > 0 {
        components.push(ComponentSize {
            label: format!("context files ({context_files} in ~/.hermes/context)"),
            chars: context_chars,
            bytes: context_bytes,
        });
    }

    // Working-directory AGENTS.md (the CLI auto-loads it).
    if let Ok(cwd) = std::env::current_dir() {
        for name in ["AGENTS.md", "agents.md", ".hermes.md"] {
            let path = cwd.join(name);
            if path.exists() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let c = ComponentSize::of(&format!("working-dir context ({name})"), &text);
                    components.push(c);
                }
                break;
            }
        }
    }

    (components, model)
}

/// Scan `~/.hermes/skills/**/SKILL.md`: index cost = one line per skill.
pub fn measure_skills() -> Vec<SkillSize> {
    fn visit(dir: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                let name = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push((name, path));
            }
        }
    }
    let mut found = Vec::new();
    visit(&hermes_home().join("skills"), &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
        .into_iter()
        .map(|(name, path)| {
            let md_bytes = std::fs::metadata(&path).map(|m| m.len()).ok();
            // Index line as rendered by the prompt builder: `    - name: description`.
            let index_line_bytes = format!("    - {name}").len();
            SkillSize {
                name,
                skill_md_bytes: md_bytes,
                index_line_bytes,
            }
        })
        .collect()
}

/// Measure the tool schemas exactly as the interactive app registers them.
pub fn measure_tools() -> (Vec<(String, usize)>, Vec<ToolsetSize>) {
    let registry = hermes_tools::ToolRegistry::new();
    let terminal_backend: Arc<dyn hermes_core::TerminalBackend> =
        Arc::new(hermes_environments::LocalBackend::default());
    let skill_store = Arc::new(hermes_skills::FileSkillStore::new(
        hermes_skills::FileSkillStore::default_dir(),
    ));
    let skill_provider: Arc<dyn hermes_core::SkillProvider> =
        Arc::new(hermes_skills::SkillManager::new(skill_store));
    hermes_tools::register_builtin_tools(&registry, terminal_backend, skill_provider);

    let schemas = registry.get_definitions();
    let infos = registry.list_tools();
    let toolset_of: BTreeMap<String, String> =
        infos.into_iter().map(|i| (i.name, i.toolset)).collect();

    let mut tools = Vec::new();
    let mut toolsets: BTreeMap<String, ToolsetSize> = BTreeMap::new();
    for schema in &schemas {
        let json_bytes = serde_json::to_string(schema).map(|s| s.len()).unwrap_or(0);
        tools.push((schema.name.clone(), json_bytes));
        let toolset = toolset_of
            .get(&schema.name)
            .cloned()
            .unwrap_or_else(|| "(unknown)".to_string());
        let entry = toolsets.entry(toolset.clone()).or_insert(ToolsetSize {
            toolset,
            tool_count: 0,
            json_bytes: 0,
        });
        entry.tool_count += 1;
        entry.json_bytes += json_bytes;
    }
    tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut toolsets: Vec<ToolsetSize> = toolsets.into_values().collect();
    toolsets.sort_by(|a, b| {
        b.json_bytes
            .cmp(&a.json_bytes)
            .then(a.toolset.cmp(&b.toolset))
    });
    (tools, toolsets)
}

/// Compute the full breakdown for a fresh session.
pub fn compute_breakdown(platform: &str) -> PromptBreakdown {
    let (components, model) = measure_components();
    let skills = measure_skills();
    let (tools, toolsets) = measure_tools();
    let system_prompt_bytes: usize = components.iter().map(|c| c.bytes).sum::<usize>()
        + skills.iter().map(|s| s.index_line_bytes).sum::<usize>();
    let tool_schema_bytes: usize = tools.iter().map(|(_, b)| b).sum();
    PromptBreakdown {
        platform: platform.to_string(),
        model,
        components,
        tools,
        toolsets,
        skills,
        system_prompt_bytes,
        tool_schema_bytes,
    }
}

fn fmt_kb(n: usize) -> String {
    format!("{:.1} KB", n as f64 / 1024.0)
}

fn render_human(b: &PromptBreakdown) -> String {
    let mut lines = vec![
        format!(
            "Prompt-size breakdown (platform={}, model={})",
            b.platform,
            if b.model.is_empty() {
                "unset"
            } else {
                &b.model
            }
        ),
        String::new(),
        format!(
            "  System prompt total : {:>8} B  ({}, estimated)",
            b.system_prompt_bytes,
            fmt_kb(b.system_prompt_bytes)
        ),
        String::new(),
        "  Static components:".to_string(),
    ];
    for c in &b.components {
        lines.push(format!(
            "    {:<44}: {:>8} B  ({})",
            c.label,
            c.bytes,
            fmt_kb(c.bytes)
        ));
    }
    let skills_index: usize = b.skills.iter().map(|s| s.index_line_bytes).sum();
    lines.push(format!(
        "    {:<44}: {:>8} B  ({})",
        format!("skills index ({} skills)", b.skills.len()),
        skills_index,
        fmt_kb(skills_index)
    ));
    lines.push(String::new());
    lines.push(format!(
        "  Tool schemas         : {:>8} B  ({}, {} tools)",
        b.tool_schema_bytes,
        fmt_kb(b.tool_schema_bytes),
        b.tools.len()
    ));
    if !b.toolsets.is_empty() {
        lines.push(String::new());
        lines.push("  Toolsets by size (tool-schema JSON, largest first):".to_string());
        lines.push(format!(
            "    {:<22} {:>5}  {:>10}",
            "toolset", "tools", "schema"
        ));
        for ts in b.toolsets.iter().take(20) {
            lines.push(format!(
                "    {:<22} {:>5}  {:>8} B  ({})",
                ts.toolset,
                ts.tool_count,
                ts.json_bytes,
                fmt_kb(ts.json_bytes)
            ));
        }
    }
    if !b.skills.is_empty() {
        lines.push(String::new());
        lines.push("  Skills by size (SKILL.md on-disk = read cost; index cost = always-on bytes, largest first):".to_string());
        lines.push(format!(
            "    {:<28} {:>10}  {:>10}",
            "skill", "SKILL.md", "index cost"
        ));
        let mut skills = b.skills.clone();
        skills.sort_by(|a, b| {
            b.skill_md_bytes
                .cmp(&a.skill_md_bytes)
                .then(a.name.cmp(&b.name))
        });
        for sk in skills.iter().take(20) {
            let md = match sk.skill_md_bytes {
                Some(n) => format!("{n:>8} B"),
                None => format!("{:>10}", "n/a"),
            };
            let name: String = if sk.name.len() <= 28 {
                sk.name.clone()
            } else {
                format!("{}…", &sk.name[..27])
            };
            lines.push(format!("    {name:<28} {md}  {:>8} B", sk.index_line_bytes));
        }
        if b.skills.len() > 20 {
            lines.push(format!(
                "    … and {} more (use --json for the full list)",
                b.skills.len() - 20
            ));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "  Estimated per-request overhead: {} (system + tool schemas, before history)",
        fmt_kb(b.system_prompt_bytes + b.tool_schema_bytes)
    ));
    lines.join("\n")
}

fn render_json(b: &PromptBreakdown) -> anyhow::Result<String> {
    let doc = serde_json::json!({
        "platform": b.platform,
        "model": b.model,
        "system_prompt_bytes": b.system_prompt_bytes,
        "tool_schema_bytes": b.tool_schema_bytes,
        "components": b.components.iter().map(|c| serde_json::json!({
            "label": c.label, "chars": c.chars, "bytes": c.bytes,
        })).collect::<Vec<_>>(),
        "tools": b.tools.iter().map(|(name, bytes)| serde_json::json!({
            "name": name, "json_bytes": bytes,
        })).collect::<Vec<_>>(),
        "toolsets": b.toolsets.iter().map(|t| serde_json::json!({
            "toolset": t.toolset, "tool_count": t.tool_count, "json_bytes": t.json_bytes,
        })).collect::<Vec<_>>(),
        "skills": b.skills.iter().map(|s| serde_json::json!({
            "name": s.name, "skill_md_bytes": s.skill_md_bytes,
            "index_line_bytes": s.index_line_bytes,
        })).collect::<Vec<_>>(),
    });
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Print the prompt-size breakdown.
pub async fn run(args: &PromptSizeArgs) -> anyhow::Result<()> {
    let breakdown = compute_breakdown(&args.platform);
    if args.json {
        println!("{}", render_json(&breakdown)?);
    } else {
        println!("{}", render_human(&breakdown));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_size_counts_chars_and_bytes() {
        let c = ComponentSize::of("t", "héllo");
        assert_eq!(c.chars, 5);
        assert_eq!(c.bytes, 6);
    }

    #[test]
    fn read_file_bytes_returns_zero_for_missing() {
        assert_eq!(
            read_file_bytes(&PathBuf::from("/nonexistent/hermes-test-file")),
            0
        );
        let dir = std::env::temp_dir().join("hermes-prompt-size-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, "12345").unwrap();
        assert_eq!(read_file_bytes(&p), 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn measure_tools_registers_schemas() {
        let (tools, toolsets) = measure_tools();
        assert!(!tools.is_empty(), "expected registered tool schemas");
        assert!(!toolsets.is_empty());
        let total: usize = toolsets.iter().map(|t| t.json_bytes).sum();
        let tools_total: usize = tools.iter().map(|(_, b)| b).sum();
        assert_eq!(total, tools_total, "toolset bytes must sum to tool total");
    }

    #[test]
    fn breakdown_totals_are_consistent() {
        let b = compute_breakdown("cli");
        assert_eq!(b.platform, "cli");
        let components: usize = b.components.iter().map(|c| c.bytes).sum();
        let skills_index: usize = b.skills.iter().map(|s| s.index_line_bytes).sum();
        assert_eq!(b.system_prompt_bytes, components + skills_index);
        let tools_total: usize = b.tools.iter().map(|(_, n)| n).sum();
        assert_eq!(b.tool_schema_bytes, tools_total);
    }

    #[test]
    fn render_human_contains_sections() {
        let b = compute_breakdown("cli");
        let text = render_human(&b);
        assert!(text.contains("Prompt-size breakdown"));
        assert!(text.contains("System prompt total"));
        assert!(text.contains("Tool schemas"));
    }

    #[test]
    fn render_json_is_valid_json() {
        let b = compute_breakdown("cli");
        let text = render_json(&b).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["platform"], "cli");
        assert!(v["system_prompt_bytes"].as_u64().is_some());
    }
}
