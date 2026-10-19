//! Command approval system
//!
//! Checks whether a terminal command requires explicit user approval
//! before execution, based on dangerous command patterns.
//!
//! Hardening (Oct 2026 — hermes-agent security audit finding #1): the gate
//! never trusts the raw command string alone. Before matching, it normalizes
//! attached-quote flags, splits the line into segments, recursively unwraps
//! leading wrappers, extracts hidden payloads, and denies power-control
//! commands by name. The most severe verdict across the raw string, every
//! segment, the unwrapped command, and every extracted payload wins, so
//! obfuscating a dangerous command can only escalate its verdict.

use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// ApprovalDecision
// ---------------------------------------------------------------------------

/// Decision from the approval check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Command is safe to execute without confirmation.
    Approved,
    /// Command is denied outright.
    Denied,
    /// Command requires user confirmation before execution.
    RequiresConfirmation,
}

/// Severity ordering: Denied > RequiresConfirmation > Approved.
fn max_severity(a: ApprovalDecision, b: ApprovalDecision) -> ApprovalDecision {
    fn rank(d: &ApprovalDecision) -> u8 {
        match d {
            ApprovalDecision::Denied => 2,
            ApprovalDecision::RequiresConfirmation => 1,
            ApprovalDecision::Approved => 0,
        }
    }
    if rank(&b) > rank(&a) {
        b
    } else {
        a
    }
}

// ---------------------------------------------------------------------------
// Dangerous patterns
// ---------------------------------------------------------------------------

/// Patterns that are always denied.
static DENIED_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\brm\s+(-\w*r\w*f\w*|-\w*f\w*r\w*)\s").unwrap(),
        Regex::new(r"(?i)\brm\s+(-\w*r\w*f\w*|-\w*f\w*r\w*)\s*/").unwrap(),
        Regex::new(r"(?i)\brm\s+--no-preserve-root\s").unwrap(),
        Regex::new(r"(?i)\bmkfs\b").unwrap(),
        Regex::new(r"(?i)\bdd\s+.*of=/dev/").unwrap(),
        Regex::new(r"(?i):\(\)\s*\{").unwrap(), // classic fork bomb: :(){ :|:& };:
        Regex::new(r"(?i):()\s*>\s*/dev/").unwrap(),
        Regex::new(r"(?i)>\s*/dev/sd[a-z]").unwrap(),
        Regex::new(r"(?i)chmod\s+777\s").unwrap(),
        Regex::new(r"(?i)chmod\s+-R\s+777\s").unwrap(),
        // Power control via systemctl is denied outright: RequiresConfirmation
        // is auto-approved in agent mode, so confirmation alone is not enough.
        Regex::new(r"(?i)\bsystemctl\s+(poweroff|reboot|halt)\b").unwrap(),
    ]
});

/// Patterns that require confirmation.
static CONFIRM_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // sudo commands
        Regex::new(r"(?i)\bsudo\b").unwrap(),
        // rm -r (but not rm -rf which is denied)
        Regex::new(r"(?i)\brm\s+-[^f]*r").unwrap(),
        // System service manipulation
        Regex::new(r"(?i)\bsystemctl\s+(start|stop|restart|enable|disable)\s").unwrap(),
        // Package management
        Regex::new(r"(?i)\b(apt|apt-get|yum|dnf|pacman|brew)\s+(install|remove|purge)\b").unwrap(),
        // Network configuration
        Regex::new(r"(?i)\biptables\b").unwrap(),
        Regex::new(r"(?i)\bifconfig\s").unwrap(),
        // Process killing
        Regex::new(r"(?i)\bkill\s+-9\b").unwrap(),
        Regex::new(r"(?i)\bkillall\b").unwrap(),
        // Disk operations
        Regex::new(r"(?i)\bformat\b").unwrap(),
        // Cron modifications
        Regex::new(r"(?i)\bcrontab\s+-r\b").unwrap(),
        // Shell pipe to sh
        Regex::new(r"\|\s*(ba)?sh\b").unwrap(),
        // Curl pipe to shell
        Regex::new(r"(?i)curl\s+.*\|\s*(ba)?sh").unwrap(),
        // Writing to system directories
        Regex::new(r"(?i)>\s*/etc/").unwrap(),
        Regex::new(r"(?i)>\s*/usr/").unwrap(),
        // Docker operations that affect system
        Regex::new(r"(?i)\bdocker\s+(rm|rmi|system\s+prune)\b").unwrap(),
        // Git force push
        Regex::new(r"(?i)\bgit\s+push\s+.*--force").unwrap(),
        Regex::new(r"(?i)\bgit\s+push\s+-f\b").unwrap(),
    ]
});

/// Command names denied outright when they are the command actually executed
/// (matched by basename, case-insensitive, after wrapper unwrapping).
static DENIED_COMMAND_NAMES: &[&str] = &["shutdown", "reboot", "poweroff", "halt"];

// ---------------------------------------------------------------------------
// Command analysis helpers (hardening)
// ---------------------------------------------------------------------------

/// Basename of a command token: `/sbin/shutdown` → `shutdown`.
fn basename_of(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Normalize attached-quote flags so `bash -c'payload'` is seen as
/// `bash -c 'payload'` by the tokenizer.
fn normalize_attached_quotes(cmd: &str) -> String {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(-c|--command)(['"])"#).unwrap());
    RE.replace_all(cmd, "$1 $2").into_owned()
}

/// Split a command line into segments on `;`, `&&`, `||`, `|` and newlines,
/// ignoring separators inside single/double quotes.
fn split_segments(cmd: &str) -> Vec<String> {
    fn push_segment(segments: &mut Vec<String>, cur: &mut String) {
        let s = cur.trim();
        if !s.is_empty() {
            segments.push(s.to_string());
        }
        cur.clear();
    }

    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(ch);
            }
            ';' | '\n' if !in_single && !in_double => push_segment(&mut segments, &mut cur),
            '&' if !in_single && !in_double => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                push_segment(&mut segments, &mut cur);
            }
            '|' if !in_single && !in_double => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                push_segment(&mut segments, &mut cur);
            }
            _ => cur.push(ch),
        }
    }
    push_segment(&mut segments, &mut cur);
    segments
}

/// Split a segment into whitespace-separated tokens, respecting quotes
/// (quote characters themselves are consumed) and backslash escapes.
/// Shell operators are dropped; segmentation handles them.
fn split_tokens(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_token = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                in_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                in_token = true;
            }
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    in_token = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            ';' | '&' | '|' | '(' | ')' | '<' | '>' if !in_single && !in_double => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            _ => {
                cur.push(ch);
                in_token = true;
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

/// Wrappers peeled (with their option arguments) to reveal the real command.
fn is_wrapper(name: &str) -> bool {
    matches!(
        name,
        "timeout"
            | "nice"
            | "stdbuf"
            | "nohup"
            | "setsid"
            | "chrt"
            | "ionice"
            | "taskset"
            | "unshare"
            | "nsenter"
            | "flock"
            | "prlimit"
            | "env"
            | "xargs"
            | "command"
            | "builtin"
            | "screen"
            | "tmux"
    )
}

/// Wrapper option flags that consume the following token as their value.
fn wrapper_value_opts(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "timeout" => &["-s", "--signal", "-k", "--kill-after"],
        "nice" => &["-n", "--adjustment"],
        "stdbuf" => &["-i", "--input", "-o", "--output", "-e", "--error"],
        "chrt" => &["-p", "--pid"],
        "ionice" => &["-c", "--class", "-n", "--classdata", "-p", "--pid"],
        "taskset" => &["-c", "--cpu-list", "-p", "--pid"],
        "xargs" => &[
            "-a", "--arg-file", "-d", "--delimiter", "-I", "--replace", "-n", "--max-args", "-P",
            "--max-procs", "-s", "--max-chars", "-E", "--eof",
        ],
        "flock" => &["-E", "--conflict-exit-code"],
        "su" | "sudo" | "doas" | "runuser" => &["-u", "--user", "-g", "--group", "-C", "--close-from"],
        _ => &[],
    }
}

/// `timeout`-style duration argument: `10`, `5s`, `1.5m`, …
fn is_duration(tok: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d+(\.\d+)?[smhd]?$").unwrap());
    RE.is_match(tok)
}

/// Recursively peel leading wrappers, returning the remaining tokens, any
/// `-c` payloads found along the way, and whether anything was peeled.
fn unwrap_wrappers(tokens: &[String]) -> (Vec<String>, Vec<String>, bool) {
    let mut rest: Vec<String> = tokens.to_vec();
    let mut payloads: Vec<String> = Vec::new();
    let mut peeled = false;
    while let Some(first_owned) = rest.first().map(|f| basename_of(f).to_ascii_lowercase()) {
        let first = first_owned.as_str();

        if first == "eval" {
            // `eval 'cmd…'` — the rest of the line IS the command.
            if rest.len() > 1 {
                payloads.push(rest[1..].join(" "));
            }
            rest.clear();
            peeled = true;
            break;
        }

        if matches!(first, "su" | "sudo" | "doas" | "runuser") {
            // Extract `-c` payloads wherever they appear …
            let mut i = 1;
            while i + 1 < rest.len() {
                if rest[i] == "-c" || rest[i] == "--command" {
                    payloads.push(rest[i + 1].clone());
                }
                i += 1;
            }
            // … then strip the wrapper and its flags to find the real command.
            let mut j = 1;
            while j < rest.len() {
                let t = rest[j].as_str();
                if t == "-c" || t == "--command" {
                    j += 2;
                } else if t == "--" {
                    j += 1;
                    break;
                } else if t.starts_with('-') && t.len() > 1 {
                    if wrapper_value_opts(first).contains(&t) {
                        j += 2;
                    } else {
                        j += 1;
                    }
                } else {
                    break;
                }
            }
            rest = rest[j.min(rest.len())..].to_vec();
            peeled = true;
            continue;
        }

        if !is_wrapper(first) {
            break;
        }

        let mut i = 1;
        while i < rest.len() {
            let t = rest[i].as_str();
            if t == "--" {
                i += 1;
                break;
            }
            if t.starts_with('-') && t.len() > 1 && !t.contains('=') {
                if wrapper_value_opts(first).contains(&t) {
                    i += 2;
                } else {
                    i += 1;
                }
            } else if first == "env" && t.contains('=') {
                i += 1;
            } else {
                break;
            }
        }
        if first == "timeout" && i < rest.len() && is_duration(&rest[i]) {
            i += 1;
        }
        if first == "chrt" && i < rest.len() && rest[i].chars().all(|c| c.is_ascii_digit()) {
            // `chrt -f <prio> <command>`
            i += 1;
        }
        rest = rest[i.min(rest.len())..].to_vec();
        peeled = true;
    }
    (rest, payloads, peeled)
}

/// Extract `system("…")` / `os.system('…')` payloads from a code fragment.
fn extract_system_payloads(code: &str) -> Vec<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(?:os\.|\b)system\s*\(\s*["']([^"']+)["']"#).unwrap()
    });
    RE.captures_iter(code)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// Extract hidden command payloads from a token list:
/// - `sh|bash|… -c|--command 'payload'`
/// - `git -c key=value` where the value smuggles `!shell`
/// - `find … -exec cmd {} ;` / `-execdir`
/// - `awk 'prog'` programs containing `system("…")`
/// - `perl|python|ruby|node|php -e|-c|-r 'code'` containing `system("…")`
fn extract_payloads(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if tokens.is_empty() {
        return out;
    }
    let cmd = basename_of(&tokens[0]).to_ascii_lowercase();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        match cmd.as_str() {
            "sh" | "bash" | "dash" | "zsh" | "fish" | "ksh"
                if (t == "-c" || t == "--command") && i + 1 < tokens.len() =>
            {
                out.push(tokens[i + 1].clone());
                i += 2;
                continue;
            }
            "git" if (t == "-c" || t == "--config") && i + 1 < tokens.len() => {
                // `git -c 'alias.x=!shutdown now' x` — anything after `!`
                // in a -c value is executed as shell by git.
                let v = tokens[i + 1].as_str();
                if let Some(bang) = v.find('!') {
                    let payload = v[bang + 1..].trim();
                    if !payload.is_empty() {
                        out.push(payload.to_string());
                    }
                }
                i += 2;
                continue;
            }
            "find" if (t == "-exec" || t == "-execdir") && i + 1 < tokens.len() => {
                let mut j = i + 1;
                let mut parts = Vec::new();
                while j < tokens.len() && tokens[j] != ";" && tokens[j] != "+" {
                    parts.push(tokens[j].clone());
                    j += 1;
                }
                if !parts.is_empty() {
                    out.push(parts.join(" "));
                }
                i = j + 1;
                continue;
            }
            "awk" | "gawk" | "mawk" => {
                if t != "awk" && t != "gawk" && t != "mawk" && !t.starts_with('-') {
                    out.extend(extract_system_payloads(t));
                }
            }
            "perl" | "python" | "python3" | "ruby" | "node" | "php"
                if (t == "-e" || t == "-c" || t == "-r") && i + 1 < tokens.len() =>
            {
                out.extend(extract_system_payloads(&tokens[i + 1]));
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// ApprovalManager
// ---------------------------------------------------------------------------

/// Manages command approval checks.
pub struct ApprovalManager {
    /// Custom denied patterns (compiled regexes).
    denied_patterns: Vec<Regex>,
    /// Custom confirm patterns (compiled regexes).
    confirm_patterns: Vec<Regex>,
}

impl ApprovalManager {
    /// Create a new ApprovalManager with built-in patterns.
    pub fn new() -> Self {
        Self {
            denied_patterns: Vec::new(),
            confirm_patterns: Vec::new(),
        }
    }

    /// Add a custom denied pattern.
    pub fn add_denied_pattern(&mut self, pattern: &str) -> Result<(), regex::Error> {
        let re = Regex::new(pattern)?;
        self.denied_patterns.push(re);
        Ok(())
    }

    /// Add a custom confirm-required pattern.
    pub fn add_confirm_pattern(&mut self, pattern: &str) -> Result<(), regex::Error> {
        let re = Regex::new(pattern)?;
        self.confirm_patterns.push(re);
        Ok(())
    }

    /// Check whether a command requires approval.
    ///
    /// Returns:
    /// - `Denied` if the command (or any hidden payload inside it) matches a
    ///   denied pattern or resolves to a denied command name
    /// - `RequiresConfirmation` if it matches a confirm pattern
    /// - `Approved` if no patterns match
    pub fn check_approval(&self, command: &str) -> ApprovalDecision {
        let normalized = normalize_attached_quotes(command);
        self.check_approval_depth(&normalized, 0)
    }

    /// Recursive worker: most severe verdict across the raw string, every
    /// `;`/`&&`/`||`/`|` segment and every extracted payload wins.
    fn check_approval_depth(&self, command: &str, depth: u32) -> ApprovalDecision {
        let mut decision = self.check_raw_patterns(command);
        if decision == ApprovalDecision::Denied || depth >= 4 {
            return decision;
        }
        for segment in split_segments(command) {
            let d = self.check_segment(&segment, depth);
            decision = max_severity(decision, d);
            if decision == ApprovalDecision::Denied {
                break;
            }
        }
        decision
    }

    /// Check one `;`-separated segment: raw patterns, wrapper unwrapping,
    /// denied command names, then hidden payloads.
    fn check_segment(&self, segment: &str, depth: u32) -> ApprovalDecision {
        let mut decision = self.check_raw_patterns(segment);
        if decision == ApprovalDecision::Denied {
            return decision;
        }
        let tokens = split_tokens(segment);
        if tokens.is_empty() {
            return decision;
        }
        let (rest, mut payloads, peeled) = unwrap_wrappers(&tokens);
        payloads.extend(extract_payloads(&tokens));
        if peeled {
            // Payloads may hide behind wrappers that were just peeled,
            // e.g. `sudo bash -c '…'`.
            payloads.extend(extract_payloads(&rest));
        }
        if !rest.is_empty() {
            if let Some(first) = rest.first() {
                let base = basename_of(first).to_ascii_lowercase();
                if DENIED_COMMAND_NAMES.contains(&base.as_str()) {
                    return ApprovalDecision::Denied;
                }
            }
        }
        for payload in payloads {
            let d = self.check_approval_depth(&payload, depth + 1);
            decision = max_severity(decision, d);
            if decision == ApprovalDecision::Denied {
                break;
            }
        }
        decision
    }

    /// The original raw-string pattern matching, unchanged.
    fn check_raw_patterns(&self, command: &str) -> ApprovalDecision {
        // Check denied patterns first (built-in then custom)
        for re in DENIED_PATTERNS.iter() {
            if re.is_match(command) {
                return ApprovalDecision::Denied;
            }
        }
        for re in &self.denied_patterns {
            if re.is_match(command) {
                return ApprovalDecision::Denied;
            }
        }

        // Check confirm patterns (built-in then custom)
        for re in CONFIRM_PATTERNS.iter() {
            if re.is_match(command) {
                return ApprovalDecision::RequiresConfirmation;
            }
        }
        for re in &self.confirm_patterns {
            if re.is_match(command) {
                return ApprovalDecision::RequiresConfirmation;
            }
        }

        ApprovalDecision::Approved
    }

    /// Async version of check_approval (same logic, for trait compatibility).
    pub async fn check_approval_async(&self, command: &str) -> ApprovalDecision {
        self.check_approval(command)
    }
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function: check if a command requires approval.
pub fn check_approval(command: &str) -> ApprovalDecision {
    let manager = ApprovalManager::new();
    manager.check_approval(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approved_commands() {
        assert_eq!(check_approval("ls -la"), ApprovalDecision::Approved);
        assert_eq!(check_approval("echo hello"), ApprovalDecision::Approved);
        assert_eq!(check_approval("cat file.txt"), ApprovalDecision::Approved);
        assert_eq!(check_approval("git status"), ApprovalDecision::Approved);
    }

    #[test]
    fn test_denied_commands() {
        assert_eq!(check_approval("rm -rf /"), ApprovalDecision::Denied);
        assert_eq!(check_approval("rm -fr /home"), ApprovalDecision::Denied);
        assert_eq!(
            check_approval("mkfs.ext4 /dev/sda1"),
            ApprovalDecision::Denied
        );
        assert_eq!(
            check_approval("chmod 777 /etc/passwd"),
            ApprovalDecision::Denied
        );
    }

    #[test]
    fn test_requires_confirmation() {
        assert_eq!(
            check_approval("sudo apt install something"),
            ApprovalDecision::RequiresConfirmation
        );
        assert_eq!(
            check_approval("systemctl restart nginx"),
            ApprovalDecision::RequiresConfirmation
        );
        assert_eq!(
            check_approval("kill -9 1234"),
            ApprovalDecision::RequiresConfirmation
        );
    }

    #[test]
    fn test_custom_patterns() {
        let mut manager = ApprovalManager::new();
        manager
            .add_denied_pattern(r"(?i)\bdangerous_cmd\b")
            .unwrap();
        manager
            .add_confirm_pattern(r"(?i)\bcautious_cmd\b")
            .unwrap();

        assert_eq!(
            manager.check_approval("dangerous_cmd"),
            ApprovalDecision::Denied
        );
        assert_eq!(
            manager.check_approval("cautious_cmd"),
            ApprovalDecision::RequiresConfirmation
        );
        assert_eq!(
            manager.check_approval("safe_cmd"),
            ApprovalDecision::Approved
        );
    }

    // -----------------------------------------------------------------------
    // Hardening battery (Oct 2026 audit): bypass classes must stay blocked
    // and benign commands must stay approved.
    // -----------------------------------------------------------------------

    /// 26 dangerous commands that must be Denied outright, including the
    /// three audit bypass classes (git alias smuggling, attached-quote
    /// flags, wrapper prefixes) and the power-control family.
    #[test]
    fn test_hardline_denied() {
        let cases = [
            // pre-existing hardline
            "rm -rf /",
            "rm -fr /home/user",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda bs=1M",
            "chmod -R 777 /",
            ":(){ :|:& };:",
            // power control by command name
            "shutdown now",
            "shutdown -h +5 'maintenance'",
            "/sbin/shutdown -r now",
            "reboot",
            "poweroff",
            "halt",
            "systemctl poweroff",
            "systemctl reboot",
            // audit bypass class 1: git alias smuggling
            "git -c 'alias.x=!shutdown now' x",
            "git -c alias.prune='!rm -rf /tmp/junk' prune",
            // audit bypass class 2: attached-quote flags
            "bash -c'rm -rf /'",
            "bash -c 'mkfs.ext4 /dev/sda1'",
            "sh --command 'poweroff'",
            // audit bypass class 3: wrapper prefixes
            "timeout 1 shutdown now",
            "sudo timeout 5 rm -rf /",
            "env FOO=bar shutdown now",
            "nice -n 10 reboot",
            // hidden payloads
            "find / -name core -exec rm -rf {} \\;",
            "find /tmp -exec shutdown now \\;",
            "awk 'BEGIN{system(\"shutdown now\")}'",
        ];
        assert_eq!(cases.len(), 26);
        for cmd in cases {
            assert_eq!(
                check_approval(cmd),
                ApprovalDecision::Denied,
                "must be denied: {cmd}"
            );
        }
    }

    /// 15 wrapper-obfuscated variants that must still be Denied.
    #[test]
    fn test_wrapper_denied() {
        let cases = [
            "timeout 10 rm -rf /",
            "timeout -s KILL 5s mkfs.ext4 /dev/sda1",
            "nice rm -rf /",
            "stdbuf -o0 shutdown now",
            "env -i reboot",
            "sudo rm -rf /",
            "su -c 'poweroff'",
            "nohup halt",
            "setsid shutdown now",
            "chrt -f 10 reboot",
            "ionice -c2 -n0 poweroff",
            "taskset -c 0 shutdown now",
            "eval 'rm -rf /'",
            "command rm -rf /",
            "builtin eval 'shutdown now'",
        ];
        assert_eq!(cases.len(), 15);
        for cmd in cases {
            assert_eq!(
                check_approval(cmd),
                ApprovalDecision::Denied,
                "must be denied: {cmd}"
            );
        }
    }

    /// The exact audit bypass shapes, end to end — plus proof that benign
    /// payloads inside the same constructs stay approved.
    #[test]
    fn test_audit_bypass_end_to_end() {
        assert_eq!(
            check_approval("git -c 'alias.x=!shutdown now' status"),
            ApprovalDecision::Denied
        );
        assert_eq!(
            check_approval("bash -c'shutdown now'"),
            ApprovalDecision::Denied
        );
        assert_eq!(
            check_approval("timeout 1 shutdown now"),
            ApprovalDecision::Denied
        );
        // Same constructs, harmless payload: must stay approved.
        assert_eq!(
            check_approval("bash -c'echo hello'"),
            ApprovalDecision::Approved
        );
        assert_eq!(
            check_approval("timeout 5 cargo test"),
            ApprovalDecision::Approved
        );
    }

    /// 35 benign commands: the hardening must not change their verdict.
    #[test]
    fn test_benign_unchanged() {
        let cases = [
            "ls -la",
            "echo hello",
            "cat file.txt",
            "git status",
            "git log --oneline -5",
            "git diff --stat",
            "cargo build --release",
            "cargo test -p hermes-tools",
            "ls /tmp",
            "pwd",
            "whoami",
            "date",
            "uname -a",
            "ps aux",
            "df -h /",
            "du -sh .",
            "head -n 20 Cargo.toml",
            "tail -n 5 Cargo.toml",
            "wc -l README.md",
            "grep -rn \"foo\" crates/",
            "find . -name \"*.rs\" | head",
            "sort Cargo.lock | uniq -c",
            "tar -czf backup.tgz src/",
            "unzip -l archive.zip",
            "curl -s https://example.com",
            "wget -q https://example.com/f",
            "ssh user@host \"uptime\"",
            "scp file.txt host:/tmp/",
            "rsync -av src/ dst/",
            "python3 scripts/build.py",
            "node --version",
            "npm --version",
            "docker ps",
            "kubectl get pods -n default",
            "vim README.md",
        ];
        assert_eq!(cases.len(), 35);
        for cmd in cases {
            assert_eq!(
                check_approval(cmd),
                ApprovalDecision::Approved,
                "must stay approved: {cmd}"
            );
        }
    }

    /// Previously-confirm commands keep their verdict.
    #[test]
    fn test_confirm_unchanged() {
        let cases = [
            "sudo apt install something",
            "sudo -u root apt install something",
            "systemctl restart nginx",
            "kill -9 1234",
            "git push --force origin main",
        ];
        for cmd in cases {
            assert_eq!(
                check_approval(cmd),
                ApprovalDecision::RequiresConfirmation,
                "must stay RequiresConfirmation: {cmd}"
            );
        }
    }
}
