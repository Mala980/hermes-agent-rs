//! Local terminal backend – executes commands on the same host.

use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use tokio::process::Command as TokioCommand;

use hermes_core::{redact_secrets, sanitize_spawn_env, AgentError, CommandOutput, TerminalBackend};

/// Result of probing `bash --version`, computed once per process.
static BASH_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether `bash` is installed (probed once per process via
/// `bash --version`); used to pick the default shell for exec.
fn bash_available() -> bool {
    *BASH_AVAILABLE.get_or_init(|| {
        std::process::Command::new("bash")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Shell argv for spawn-per-call execution: `bash -c` like the original
/// Hermes, falling back to `sh -c` (with a warning) when bash is not
/// installed. The `available` parameter is the probed result; kept separate
/// so the selection itself is unit-testable.
fn shell_argv(command: &str) -> (String, Vec<String>) {
    shell_argv_with(bash_available(), command)
}

fn shell_argv_with(available: bool, command: &str) -> (String, Vec<String>) {
    if available {
        (
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    } else {
        tracing::warn!(
            "bash not found on PATH; exec falls back to `sh -c` \
             (bash-isms like [[ ]], arrays, (( )) are unsupported)"
        );
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
}

/// A [`TerminalBackend`] that runs commands on the local machine.
pub struct LocalBackend {
    /// Default timeout in seconds for command execution.
    default_timeout: u64,
    /// Maximum output size in bytes before truncation.
    max_output_size: usize,
    /// Background processes tracked by PID for potential later interaction.
    background_processes: Arc<Mutex<Vec<u32>>>,
}

impl LocalBackend {
    /// Create a new local backend with the given defaults.
    pub fn new(default_timeout: u64, max_output_size: usize) -> Self {
        Self {
            default_timeout,
            max_output_size,
            background_processes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        // Reference parity: the original's exec default timeout is 180s.
        Self::new(180, 1_048_576)
    }
}

#[async_trait]
impl TerminalBackend for LocalBackend {
    async fn execute_command(
        &self,
        command: &str,
        timeout: Option<u64>,
        workdir: Option<&str>,
        background: bool,
        pty: bool,
    ) -> Result<CommandOutput, AgentError> {
        let timeout_secs = timeout.unwrap_or(self.default_timeout);

        if pty {
            // PTY mode: allocate a pseudo-terminal for interactive commands.
            // Uses `script` command (available on macOS/Linux) to wrap the
            // command in a PTY, which makes programs behave as if connected
            // to a real terminal (enables colors, line editing, etc.).
            #[cfg(unix)]
            {
                let mut pty_cmd = TokioCommand::new("script");
                pty_cmd
                    .arg("-q") // quiet mode
                    .arg("/dev/null") // discard typescript file
                    .arg("-c") // command to execute
                    .arg(command)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null());

                if let Some(dir) = workdir {
                    pty_cmd.current_dir(dir);
                }

                // Security: strip credential env vars from the child and
                // redact any secrets from its captured output.
                sanitize_spawn_env(pty_cmd.as_std_mut());

                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
                        let output = pty_cmd.output().await.map_err(|e| {
                            AgentError::Io(format!("Failed to spawn PTY command: {}", e))
                        })?;
                        let stdout = redact_secrets(&String::from_utf8_lossy(&output.stdout));
                        let stderr = redact_secrets(&String::from_utf8_lossy(&output.stderr));
                        Ok(CommandOutput {
                            exit_code: output.status.code().unwrap_or(-1),
                            stdout: if stdout.len() > self.max_output_size {
                                stdout[..self.max_output_size].to_string()
                            } else {
                                stdout
                            },
                            stderr: if stderr.len() > self.max_output_size {
                                stderr[..self.max_output_size].to_string()
                            } else {
                                stderr
                            },
                        })
                    })
                    .await;

                return match result {
                    Ok(Ok(output)) => Ok(output),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(AgentError::Timeout(format!(
                        "PTY command timed out after {} seconds",
                        timeout_secs
                    ))),
                };
            }

            #[cfg(not(unix))]
            {
                tracing::warn!("PTY mode is not supported on this platform; falling back to standard execution");
            }
        }

        // Default `bash -c` like the original Hermes, falling back to
        // `sh -c` when bash is not installed.
        let (shell_prog, shell_args) = shell_argv(command);
        let mut cmd = TokioCommand::new(&shell_prog);
        cmd.args(&shell_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        if background {
            // In background mode, detach the process and return immediately.
            cmd.stdin(Stdio::null());
        }

        // Security: strip credential env vars from the child and redact any
        // secrets from its captured output.
        sanitize_spawn_env(cmd.as_std_mut());

        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let child = cmd
                .spawn()
                .map_err(|e| AgentError::Io(format!("Failed to spawn command: {}", e)))?;

            // In background mode, track the PID and return immediately.
            if background {
                if let Some(id) = child.id() {
                    self.background_processes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(id);
                }
                return Ok(CommandOutput {
                    exit_code: 0,
                    stdout: format!(
                        "Process started in background (pid: {})",
                        child.id().unwrap_or(0)
                    ),
                    stderr: String::new(),
                });
            }

            let output = child
                .wait_with_output()
                .await
                .map_err(|e| AgentError::Io(format!("Failed to wait for command: {}", e)))?;

            let stdout = redact_secrets(&String::from_utf8_lossy(&output.stdout));
            let stderr = redact_secrets(&String::from_utf8_lossy(&output.stderr));

            // Truncate output if it exceeds max_output_size
            let stdout = if stdout.len() > self.max_output_size {
                tracing::warn!(
                    "Command output exceeded max size ({} bytes), truncating",
                    stdout.len()
                );
                stdout[..self.max_output_size].to_string()
            } else {
                stdout
            };

            let stderr = if stderr.len() > self.max_output_size {
                tracing::warn!(
                    "Command stderr exceeded max size ({} bytes), truncating",
                    stderr.len()
                );
                stderr[..self.max_output_size].to_string()
            } else {
                stderr
            };

            Ok(CommandOutput {
                exit_code: output.status.code().unwrap_or(-1),
                stdout,
                stderr,
            })
        })
        .await;

        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(AgentError::Timeout(format!(
                "Command timed out after {} seconds",
                timeout_secs
            ))),
        }
    }

    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<String, AgentError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| AgentError::Io(format!("Failed to read file '{}': {}", path, e)))?;

        let lines: Vec<&str> = content.lines().collect();

        // Apply offset (0-indexed line number to start from)
        let start = offset.unwrap_or(0) as usize;
        let start = start.min(lines.len());

        // Apply limit (max number of lines to return)
        let end = if let Some(lim) = limit {
            (start + lim as usize).min(lines.len())
        } else {
            lines.len()
        };

        let selected_lines = &lines[start..end];
        Ok(selected_lines.join("\n"))
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), AgentError> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    AgentError::Io(format!(
                        "Failed to create parent directory for '{}': {}",
                        path, e
                    ))
                })?;
            }
        }

        tokio::fs::write(path, content)
            .await
            .map_err(|e| AgentError::Io(format!("Failed to write file '{}': {}", path, e)))?;

        Ok(())
    }

    async fn file_exists(&self, path: &str) -> Result<bool, AgentError> {
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(AgentError::Io(format!(
                "Failed to check file existence '{}': {}",
                path, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_argv_prefers_bash() {
        let (prog, args) = shell_argv_with(true, "echo hi");
        assert_eq!(prog, "bash");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn test_shell_argv_falls_back_to_sh() {
        let (prog, args) = shell_argv_with(false, "echo hi");
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn test_shell_argv_matches_probe() {
        let (prog, _) = shell_argv("echo hi");
        assert_eq!(prog, if bash_available() { "bash" } else { "sh" });
    }

    /// A bash-ism (`[[ ]]`) only runs under the preferred `bash -c` shell.
    #[tokio::test]
    async fn test_local_backend_uses_bash_when_available() {
        if !bash_available() {
            return; // Degenerate environment: nothing to prove.
        }
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("[[ 1 -eq 1 ]] && echo bash-ok", None, None, false, false)
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("bash-ok"));
    }

    #[tokio::test]
    async fn test_execute_command_echo() {
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("echo hello", None, None, false, false)
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.trim().contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_command_with_workdir() {
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("pwd", None, Some("/tmp"), false, false)
            .await
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.trim().contains("/tmp"));
    }

    #[tokio::test]
    async fn test_execute_command_timeout() {
        let backend = LocalBackend::new(1, 1_048_576);
        let result = backend
            .execute_command("sleep 30", None, None, false, false)
            .await;
        assert!(result.is_err());
        match result {
            Err(AgentError::Timeout(_)) => {}
            _ => panic!("Expected timeout error"),
        }
    }

    #[tokio::test]
    async fn test_execute_command_failure() {
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("exit 42", None, None, false, false)
            .await
            .unwrap();
        assert_eq!(output.exit_code, 42);
    }

    #[tokio::test]
    async fn test_write_and_read_file() {
        let dir = std::env::temp_dir().join("hermes_test_write_read");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_file.txt");
        let path_str = path.to_string_lossy().to_string();

        let backend = LocalBackend::default();

        backend
            .write_file(&path_str, "hello\nworld\nfoo\nbar")
            .await
            .unwrap();
        let content = backend.read_file(&path_str, None, None).await.unwrap();
        assert_eq!(content, "hello\nworld\nfoo\nbar");

        // Test with offset
        let content = backend.read_file(&path_str, Some(1), None).await.unwrap();
        assert_eq!(content, "world\nfoo\nbar");

        // Test with offset and limit
        let content = backend
            .read_file(&path_str, Some(1), Some(2))
            .await
            .unwrap();
        assert_eq!(content, "world\nfoo");

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn test_file_exists() {
        let backend = LocalBackend::default();

        // A path that should exist
        assert!(backend.file_exists("/tmp").await.unwrap());

        // A path that should not exist
        assert!(!backend
            .file_exists("/tmp/hermes_nonexistent_test_file_xyz")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_execute_command_strips_credential_env_vars() {
        // Unique names so parallel tests cannot collide with them.
        std::env::set_var("HERMES_LOCAL_TEST_STRIP_API_KEY", "SHOULDNOTLEAK1");
        std::env::set_var("HERMES_LOCAL_TEST_KEEP", "keepme");
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("env", None, None, false, false)
            .await
            .unwrap();
        std::env::remove_var("HERMES_LOCAL_TEST_STRIP_API_KEY");
        std::env::remove_var("HERMES_LOCAL_TEST_KEEP");

        assert!(
            !output
                .stdout
                .lines()
                .any(|l| l.starts_with("HERMES_LOCAL_TEST_STRIP_API_KEY=")),
            "credential env var leaked into child"
        );
        assert!(
            !output.stdout.contains("SHOULDNOTLEAK1"),
            "credential value leaked into child"
        );
        assert!(
            output
                .stdout
                .lines()
                .any(|l| l == "HERMES_LOCAL_TEST_KEEP=keepme"),
            "normal env var must pass through"
        );
        assert!(
            output.stdout.lines().any(|l| l.starts_with("PATH=")),
            "PATH must pass through"
        );
    }

    #[tokio::test]
    async fn test_execute_command_redacts_secrets_in_output() {
        let backend = LocalBackend::default();
        let output = backend
            .execute_command("echo 'api_key=hunter2value'", None, None, false, false)
            .await
            .unwrap();
        assert!(
            !output.stdout.contains("hunter2value"),
            "secret leaked in stdout"
        );
        assert!(output.stdout.contains("[REDACTED]"));

        let output = backend
            .execute_command("echo hello", None, None, false, false)
            .await
            .unwrap();
        assert!(output.stdout.contains("hello"));
    }
}
