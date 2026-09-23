use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `/home/me/.hey` as `~/.hey`, for messages.
pub fn display_path(p: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = p.strip_prefix(&home)
    {
        let sep = if cfg!(windows) { "\\" } else { "/" };
        return format!("~{sep}{}", rest.display());
    }
    p.display().to_string()
}

pub struct Output {
    pub success: bool,
    pub stdout: String,
}

/// Run a command, killing it if it outlives `timeout`. Returns `None` on
/// spawn failure or timeout. stdin is closed, stderr is discarded.
pub fn run_timeout(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut pipe = child.stdout.take()?;
    // Drain on a thread so a chatty child cannot block on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let buf = reader.join().ok()?;
    Some(Output {
        success: status.success(),
        stdout: String::from_utf8_lossy(&buf).into_owned(),
    })
}

/// The platform shell invocation for a one-line command string.
pub fn shell_command(line: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(line);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(line);
        c
    }
}

/// Walk up from `start` looking for a `.git` entry.
pub fn inside_git_tree(start: &Path) -> bool {
    start.ancestors().any(|d| d.join(".git").exists())
}

pub fn basename(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.strip_suffix(".exe").unwrap_or(name)
}

pub fn is_truthy(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_handles_paths_and_exe() {
        assert_eq!(basename("/usr/bin/zsh"), "zsh");
        assert_eq!(basename("C:\\Program Files\\PowerShell\\pwsh.exe"), "pwsh");
        assert_eq!(basename("bash"), "bash");
    }

    #[test]
    fn truthy() {
        assert_eq!(is_truthy("Yes"), Some(true));
        assert_eq!(is_truthy("off"), Some(false));
        assert_eq!(is_truthy("maybe"), None);
    }
}
