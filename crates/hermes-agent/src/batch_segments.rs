//! Tool-call batch segmentation with read/write barriers.
//!
//! Parity port of hermes-rs `batch_segments.rs` (W1-B4), itself a port of
//! `_plan_tool_batch_segments` (`agent/tool_dispatch_helpers.py`) in the
//! original Python agent.
//!
//! A model turn's tool calls are split into ordered `(kind, calls)` segments
//! in emission order:
//! - consecutive readers accumulate into a parallel run (executed with
//!   `JoinSet`, results re-associated by tool-call id);
//! - a writer closes the pending reader run and then runs alone;
//! - a read of a path written earlier in the same batch is serialized behind
//!   the writer via the path-overlap barrier.
//!
//! Result order per call and side-effect boundaries match fully-sequential
//! execution (original: "result order and side-effect boundaries match
//! fully-sequential execution").
//!
//! Read/write classification is derived from hermes-rs's `Tool::effect`
//! annotations (W1-B4), mapped onto this fork's tool names — this crate's
//! tool registry carries no mutating/readonly flag, so a name table is used
//! instead of per-tool annotations. Unknown tools are treated as writers
//! (conservative barrier, like the original).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use hermes_core::ToolCall;

/// Kind of a planned batch segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Calls in the segment may run concurrently (results are still matched
    /// back to calls by tool-call id).
    Parallel,
    /// Calls in the segment run one at a time, in order.
    Sequential,
}

/// Read/write classification for barrier planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolEffect {
    Read,
    Write,
}

/// Filesystem tools whose `path` argument scopes their effect (`search_files`
/// defaults to the working dir when `path` is absent, like `grep`/`glob` in
/// hermes-rs): reader<->reader may share a subtree, any overlap involving a
/// writer closes the run so the conflicting call waits in a later segment.
const PATH_SCOPED_TOOLS: &[&str] = &["read_file", "write_file", "patch", "search_files"];

/// Read-only tools: safe to run in parallel with each other. Derived from
/// hermes-rs's `Tool::effect` annotations (W1-B4), mapped to this fork's
/// tool names. Everything not listed here — including unknown tools — is
/// treated as a writer (conservative barrier, like the original).
const READ_TOOLS: &[&str] = &[
    "browser_console",
    "browser_get_images",
    "browser_snapshot",
    "browser_vision",
    "credential_files",
    "env_passthrough",
    "feishu_doc_read",
    "ha_get_state",
    "ha_list_entities",
    "ha_list_services",
    "osv_check",
    "read_file",
    "read_terminal",
    "read_window_below",
    "search_files",
    "session_search",
    "skill_view",
    "skills_list",
    "transcription",
    "url_safety",
    "video_analyze",
    "video_status",
    "vision_analyze",
    "web_extract",
    "web_search",
    "x_search",
];

/// Working directory used to resolve relative tool paths. Same convention
/// as the rest of the agent loop (`TERMINAL_CWD`, else process cwd).
pub fn resolve_cwd() -> PathBuf {
    std::env::var("TERMINAL_CWD")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Split a batch of tool calls into ordered `(kind, calls)` segments in
/// emission order — mirrors `_plan_tool_batch_segments`.
///
/// Rules (same as hermes-rs/original):
/// - Non-path-scoped writers, non-object arguments, and path-scoped tools
///   with unknown scope are sequential barriers.
/// - A path-scoped tool joins the parallel run only if it does not conflict
///   with the run's reservations: reader<->reader may overlap; any overlap
///   involving a writer closes the run so the call starts a NEW run after
///   the conflicting call completes.
/// - Runs shorter than two calls demote to sequential; adjacent sequential
///   segments merge.
pub fn plan_tool_batch_segments(
    tool_calls: &[ToolCall],
    cwd: &Path,
) -> Vec<(SegmentKind, Vec<ToolCall>)> {
    let mut segments: Vec<(SegmentKind, Vec<ToolCall>)> = Vec::new();
    let mut current: Vec<ToolCall> = Vec::new();
    let mut reserved: Vec<(PathBuf, bool)> = Vec::new(); // (canonical path, is_writer)

    for call in tool_calls {
        match batch_admission(call, cwd) {
            None => {
                close_run(&mut segments, &mut current);
                reserved.clear();
                push_sequential(&mut segments, [call.clone()]);
            }
            Some((scoped, is_writer)) => {
                let conflict = scoped.iter().any(|path| {
                    reserved.iter().any(|(existing, existing_is_writer)| {
                        (is_writer || *existing_is_writer) && paths_overlap(path, existing)
                    })
                });
                if conflict {
                    close_run(&mut segments, &mut current);
                    reserved.clear();
                }
                reserved.extend(scoped.into_iter().map(|p| (p, is_writer)));
                current.push(call.clone());
            }
        }
    }
    close_run(&mut segments, &mut current);
    segments
}

/// Close the running parallel run: runs of >= 2 calls become a parallel
/// segment, a lone call demotes to sequential (original: `_close_parallel`).
fn close_run(segments: &mut Vec<(SegmentKind, Vec<ToolCall>)>, current: &mut Vec<ToolCall>) {
    if current.len() >= 2 {
        segments.push((SegmentKind::Parallel, std::mem::take(current)));
    } else if !current.is_empty() {
        push_sequential(segments, std::mem::take(current));
    }
}

/// Append calls to a sequential segment, merging with an adjacent sequential
/// segment when present (original: `_extend_sequential`).
fn push_sequential(
    segments: &mut Vec<(SegmentKind, Vec<ToolCall>)>,
    calls: impl IntoIterator<Item = ToolCall>,
) {
    let mut calls = calls.into_iter().peekable();
    if calls.peek().is_none() {
        return;
    }
    if let Some((SegmentKind::Sequential, tail)) = segments.last_mut() {
        tail.extend(calls);
    } else {
        segments.push((SegmentKind::Sequential, calls.collect()));
    }
}

/// Classify one call for the planner: `None` = sequential barrier, otherwise
/// `(scoped_paths, is_writer)` (empty paths = safely-parallel reader with no
/// scope). (original: `_batch_admission`)
fn batch_admission(call: &ToolCall, cwd: &Path) -> Option<(Vec<PathBuf>, bool)> {
    let name = call.function.name.as_str();
    let args: serde_json::Value = serde_json::from_str(&call.function.arguments).ok()?;
    let args = args.as_object()?; // non-object arguments -> barrier
    if PATH_SCOPED_TOOLS.contains(&name) {
        let scoped = extract_scope_paths(name, args, cwd);
        if scoped.is_empty() {
            return None; // unknown scope -> barrier
        }
        return Some((scoped, effect_of(name) == ToolEffect::Write));
    }
    match effect_of(name) {
        // Pure reader: join the parallel run with no path reservation.
        ToolEffect::Read => Some((Vec::new(), false)),
        // Non-path-scoped writer (terminal, browser_*, memory, ...): its
        // mutation scope cannot be tracked -> sequential barrier.
        ToolEffect::Write => None,
    }
}

/// Read/write classification from the name table; unknown tools are writers
/// (conservative barrier, like the original).
fn effect_of(name: &str) -> ToolEffect {
    if READ_TOOLS.contains(&name) {
        ToolEffect::Read
    } else {
        ToolEffect::Write
    }
}

/// Every canonical path this call reserves for overlap checking.
/// (original: `_extract_parallel_scope_paths`)
fn extract_scope_paths(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cwd: &Path,
) -> Vec<PathBuf> {
    let raw: Vec<&str> = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => vec![p],
        // search_files defaults to the working dir — reserve cwd; other
        // path-scoped tools without a path have unknown scope.
        _ if name == "search_files" => vec!["."],
        _ => Vec::new(),
    };
    // Dedup keeping first-occurrence order (original: dict.fromkeys).
    let mut seen = HashSet::new();
    raw.into_iter()
        .map(|p| normalize_path(p, cwd))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Lexical canonicalization for overlap detection: expand `~`, absolutize
/// against `cwd`, clean `.`/`..` components.
///
/// The original uses realpath+normcase (needs I/O and fails for writer
/// targets that do not exist yet); this lexical form is deterministic and
/// sufficient for barrier semantics.
fn normalize_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(raw),
        }
    } else {
        PathBuf::from(raw)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

/// True when two canonical paths denote the same subtree (one is a component
/// prefix of the other). (original: `_paths_overlap`)
fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left_parts: Vec<_> = left.components().collect();
    let right_parts: Vec<_> = right.components().collect();
    if left_parts.is_empty() || right_parts.is_empty() {
        return false;
    }
    let common = left_parts.len().min(right_parts.len());
    left_parts[..common] == right_parts[..common]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            function: hermes_core::FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn plan(calls: &[ToolCall]) -> Vec<(SegmentKind, Vec<ToolCall>)> {
        plan_tool_batch_segments(calls, Path::new("/work"))
    }

    fn kinds(segments: &[(SegmentKind, Vec<ToolCall>)]) -> Vec<(SegmentKind, Vec<String>)> {
        segments
            .iter()
            .map(|(k, calls)| (*k, calls.iter().map(|c| c.id.clone()).collect()))
            .collect()
    }

    #[test]
    fn test_readers_different_paths_form_one_parallel_segment() {
        let calls = vec![
            call("a", "read_file", json!({"path": "one.txt"})),
            call("b", "read_file", json!({"path": "two.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_readers_sharing_subtree_stay_parallel() {
        let calls = vec![
            call("a", "read_file", json!({"path": "src"})),
            call(
                "b",
                "search_files",
                json!({"pattern": "x", "path": "src/lib.rs"}),
            ),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_writer_closes_previous_run() {
        // read(a) vs write(a): writer conflicts with the reader -> barrier.
        // write(a) + read(b) do not overlap -> new parallel run.
        let calls = vec![
            call("a", "read_file", json!({"path": "a.txt"})),
            call("b", "write_file", json!({"path": "a.txt", "content": "x"})),
            call("c", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![
                (SegmentKind::Sequential, vec!["a".to_string()]),
                (
                    SegmentKind::Parallel,
                    vec!["b".to_string(), "c".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn test_writer_without_overlap_stays_parallel() {
        let calls = vec![
            call("a", "write_file", json!({"path": "a.txt", "content": "x"})),
            call("b", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Parallel,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_two_writers_same_path_become_barrier() {
        let calls = vec![
            call("a", "write_file", json!({"path": "a.txt", "content": "1"})),
            call("b", "write_file", json!({"path": "a.txt", "content": "2"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_read_after_write_same_path_is_serialized() {
        // Path-overlap rule: a read of a path written earlier in the same
        // batch must wait — the write closes the run, the read waits in the
        // next segment (segment execution order guarantees the write
        // completes first).
        let calls = vec![
            call("a", "write_file", json!({"path": "f.txt", "content": "1"})),
            call("b", "read_file", json!({"path": "f.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_unknown_tool_is_write_barrier() {
        // Unknown/unregistered tools -> conservative writer barrier.
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "ghost_tool", json!({})),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn test_clarify_is_barrier_and_sequential_segments_merge() {
        let calls = vec![
            call("a", "read_file", json!({"path": "a.txt"})),
            call("b", "clarify", json!({"question": "go on?"})),
            call("c", "read_file", json!({"path": "b.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn test_non_path_scoped_writer_is_barrier() {
        // terminal (writer, untrackable scope) -> barrier; adjacent
        // sequential segments merge.
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "terminal", json!({"command": "ls"})),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn test_non_object_arguments_are_barrier() {
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            call("b", "web_search", json!("not-an-object")),
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn test_invalid_json_arguments_are_barrier() {
        let calls = vec![
            call("a", "web_search", json!({"query": "x"})),
            ToolCall {
                id: "b".to_string(),
                function: hermes_core::FunctionCall {
                    name: "web_search".to_string(),
                    arguments: "{not json".to_string(),
                },
            },
            call("c", "web_search", json!({"query": "y"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            )]
        );
    }

    #[test]
    fn test_singleton_run_demotes_to_sequential() {
        let calls = vec![call("a", "web_search", json!({"query": "x"}))];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(SegmentKind::Sequential, vec!["a".to_string()])]
        );
    }

    #[test]
    fn test_emission_order_always_preserved() {
        let calls = vec![
            call("a", "web_search", json!({"query": "1"})),
            call("b", "write_file", json!({"path": "f.txt", "content": "x"})),
            call("c", "read_file", json!({"path": "f.txt"})),
            call("d", "web_search", json!({"query": "2"})),
        ];
        let segments = plan(&calls);
        let order: Vec<&str> = segments
            .iter()
            .flat_map(|(_, cs)| cs.iter().map(|c| c.id.as_str()))
            .collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        assert_eq!(
            kinds(&segments),
            vec![
                (
                    SegmentKind::Parallel,
                    vec!["a".to_string(), "b".to_string()]
                ),
                (
                    SegmentKind::Parallel,
                    vec!["c".to_string(), "d".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn test_path_normalization_catches_equivalent_forms() {
        // ./f.txt and sub/../f.txt are the same path -> writer conflict.
        let calls = vec![
            call(
                "a",
                "write_file",
                json!({"path": "./f.txt", "content": "1"}),
            ),
            call("b", "read_file", json!({"path": "sub/../f.txt"})),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_search_files_without_path_scopes_to_cwd() {
        // search_files without `path` reserves the working dir: a writer on
        // any file under it conflicts.
        let calls = vec![
            call("a", "search_files", json!({"pattern": "x"})),
            call(
                "b",
                "write_file",
                json!({"path": "deep/f.txt", "content": "1"}),
            ),
        ];
        assert_eq!(
            kinds(&plan(&calls)),
            vec![(
                SegmentKind::Sequential,
                vec!["a".to_string(), "b".to_string()]
            )]
        );
    }

    #[test]
    fn test_effect_table_covers_known_tools() {
        // Spot-check the classification table derived from hermes-rs.
        for name in [
            "read_file",
            "search_files",
            "web_search",
            "web_extract",
            "x_search",
            "session_search",
            "transcription",
            "video_analyze",
            "vision_analyze",
            "browser_snapshot",
            "skills_list",
            "skill_view",
            "read_terminal",
            "credential_files",
            "env_passthrough",
            "osv_check",
            "url_safety",
        ] {
            assert_eq!(effect_of(name), ToolEffect::Read, "{name} should be Read");
        }
        for name in [
            "write_file",
            "patch",
            "terminal",
            "execute_code",
            "delegate_task",
            "clarify",
            "memory",
            "browser_exec",
            "browser_cdp",
            "computer_use",
            "drive_preview",
            "desktop_preview",
            "annotate_preview",
            "send_message",
            "todo",
            "cronjob",
            "image_generate",
            "video_generate",
            "text_to_speech",
            "ghost_tool",
        ] {
            assert_eq!(effect_of(name), ToolEffect::Write, "{name} should be Write");
        }
    }
}
