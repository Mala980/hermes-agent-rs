//! Real code execution backend: run scripts via tokio::process::Command.

use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command as TokioCommand;

use crate::tools::code_execution::CodeExecutionBackend;
use hermes_core::{redact_secrets, sanitize_spawn_env, ToolError};

/// Code execution backend using local interpreters.
pub struct LocalCodeExecutionBackend {
    default_timeout_secs: u64,
}

impl LocalCodeExecutionBackend {
    pub fn new(default_timeout_secs: u64) -> Self {
        Self {
            default_timeout_secs,
        }
    }
}

impl Default for LocalCodeExecutionBackend {
    fn default() -> Self {
        Self::new(30)
    }
}

#[async_trait]
impl CodeExecutionBackend for LocalCodeExecutionBackend {
    async fn execute(
        &self,
        code: &str,
        language: Option<&str>,
        timeout: Option<u64>,
    ) -> Result<String, ToolError> {
        let lang = language.unwrap_or("python");
        let timeout_secs = timeout.unwrap_or(self.default_timeout_secs);

        let (interpreter, flag) = match lang {
            "python" | "python3" => ("python3", "-c"),
            "javascript" | "js" | "node" => ("node", "-e"),
            "typescript" | "ts" => ("npx", ""),
            "bash" | "sh" => ("bash", "-c"),
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "Unsupported language: {}",
                    other
                )))
            }
        };

        let mut cmd = TokioCommand::new(interpreter);
        if lang == "typescript" {
            // For TypeScript, write to temp file and run with ts-node
            let tmp = std::env::temp_dir().join(format!("hermes_exec_{}.ts", uuid::Uuid::new_v4()));
            tokio::fs::write(&tmp, code).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to write temp file: {}", e))
            })?;
            cmd = TokioCommand::new("npx");
            cmd.arg("ts-node").arg(tmp.to_str().unwrap_or(""));
        } else {
            cmd.arg(flag).arg(code);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        // Security: strip credential env vars from the interpreter child.
        sanitize_spawn_env(cmd.as_std_mut());

        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let child = cmd.spawn().map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to spawn {}: {}", interpreter, e))
            })?;
            let output = child.wait_with_output().await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to wait for process: {}", e))
            })?;
            Ok::<_, ToolError>((
                output.status.code().unwrap_or(-1),
                output.stdout,
                output.stderr,
            ))
        })
        .await;

        match result {
            Ok(Ok((exit_code, stdout, stderr))) => {
                // Security: redact any secrets before the output becomes a
                // tool result.
                let stdout_str = redact_secrets(&String::from_utf8_lossy(&stdout));
                let stderr_str = redact_secrets(&String::from_utf8_lossy(&stderr));
                Ok(json!({
                    "exit_code": exit_code,
                    "stdout": stdout_str,
                    "stderr": stderr_str,
                    "language": lang,
                })
                .to_string())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ToolError::Timeout(format!(
                "Code execution timed out after {}s",
                timeout_secs
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_bash(code: &str) -> String {
        let backend = LocalCodeExecutionBackend::default();
        backend
            .execute(code, Some("bash"), Some(30))
            .await
            .expect("code execution failed")
    }

    #[tokio::test]
    async fn code_execution_strips_credential_env_vars() {
        // Unique names so parallel tests cannot collide with them.
        std::env::set_var("HERMES_CE_TEST_STRIP_TOKEN", "SHOULDNOTLEAK2");
        std::env::set_var("HERMES_CE_TEST_KEEP", "keepme");
        let result = run_bash(
            "echo \"v=${HERMES_CE_TEST_STRIP_TOKEN:-UNSET}\"; echo \"k=$HERMES_CE_TEST_KEEP\"",
        )
        .await;
        std::env::remove_var("HERMES_CE_TEST_STRIP_TOKEN");
        std::env::remove_var("HERMES_CE_TEST_KEEP");

        assert!(
            !result.contains("SHOULDNOTLEAK2"),
            "credential value leaked into interpreter child"
        );
        assert!(
            result.contains("v=UNSET"),
            "credential var must be stripped"
        );
        assert!(result.contains("k=keepme"), "normal var must pass through");
    }

    #[tokio::test]
    async fn code_execution_redacts_secrets_in_output() {
        let result = run_bash("echo 'api_key=hunter2value'").await;
        assert!(
            !result.contains("hunter2value"),
            "secret leaked in code output"
        );
        assert!(result.contains("[REDACTED]"));

        let result = run_bash("echo hello").await;
        assert!(result.contains("hello"));
    }
}
