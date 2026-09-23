//! Context collection: stdin, system, shell, git.

use crate::util::{basename, run_timeout};
use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- ANSI ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    Text,
    Esc,
    /// After `ESC [`: parameters until a final byte in 0x40..=0x7e.
    Csi,
    /// After `ESC ]`: until BEL or `ESC \`.
    Osc,
    OscEsc,
    /// After `ESC (`, `ESC #` and friends: one more byte follows.
    Skip1,
}

/// Removes ANSI escape sequences from a byte stream. Keeps state across
/// chunks so a sequence split over two reads is still removed.
pub struct AnsiStripper {
    state: AnsiState,
}

impl AnsiStripper {
    pub fn new() -> Self {
        AnsiStripper { state: AnsiState::Text }
    }

    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.state = match self.state {
                AnsiState::Text => {
                    if b == 0x1b {
                        AnsiState::Esc
                    } else {
                        out.push(b);
                        AnsiState::Text
                    }
                }
                AnsiState::Esc => match b {
                    b'[' => AnsiState::Csi,
                    b']' => AnsiState::Osc,
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' | b' ' => AnsiState::Skip1,
                    0x1b => AnsiState::Esc,
                    _ => AnsiState::Text,
                },
                AnsiState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        AnsiState::Text
                    } else {
                        AnsiState::Csi
                    }
                }
                AnsiState::Osc => match b {
                    0x07 => AnsiState::Text,
                    0x1b => AnsiState::OscEsc,
                    _ => AnsiState::Osc,
                },
                AnsiState::OscEsc => {
                    if b == b'\\' {
                        AnsiState::Text
                    } else {
                        AnsiState::Osc
                    }
                }
                AnsiState::Skip1 => AnsiState::Text,
            };
        }
    }
}

#[cfg(test)]
pub fn strip_ansi(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    AnsiStripper::new().feed(s.as_bytes(), &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

// --------------------------------------------------------------- stdin ---

/// Keeps the first 40% and last 60% of a byte stream of unknown length.
pub struct HeadTail {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl HeadTail {
    pub fn new(cap: usize) -> Self {
        let head_cap = cap * 2 / 5;
        HeadTail {
            head_cap,
            tail_cap: cap - head_cap,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
        }
    }

    pub fn push(&mut self, mut data: &[u8]) {
        self.total += data.len();
        if self.head.len() < self.head_cap {
            let n = (self.head_cap - self.head.len()).min(data.len());
            self.head.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
        if data.is_empty() {
            return;
        }
        self.tail.extend_from_slice(data);
        // Trim lazily so pushes stay amortised O(1).
        if self.tail.len() > self.tail_cap * 2 {
            let cut = self.tail.len() - self.tail_cap;
            self.tail.drain(..cut);
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// The kept text, with a marker line where bytes were dropped.
    pub fn finish(mut self) -> String {
        if self.tail.len() > self.tail_cap {
            let cut = self.tail.len() - self.tail_cap;
            self.tail.drain(..cut);
        }
        if self.total <= self.head_cap + self.tail_cap {
            self.head.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&self.head).into_owned();
        }
        // Cut on character boundaries so we never emit U+FFFD for our own cuts.
        if let Err(e) = std::str::from_utf8(&self.head)
            && e.error_len().is_none()
        {
            self.head.truncate(e.valid_up_to());
        }
        let skip = self.tail.iter().take_while(|&&b| b & 0xC0 == 0x80).count();
        let tail = &self.tail[skip.min(3)..];
        let dropped = self.total - self.head.len() - tail.len();
        format!(
            "{}\n[... {} bytes truncated ...]\n{}",
            String::from_utf8_lossy(&self.head),
            dropped,
            String::from_utf8_lossy(tail)
        )
    }
}

/// Read a stream, strip ANSI, and apply the size cap. Returns `None` if
/// nothing but whitespace arrived.
pub fn read_capped(mut r: impl Read, cap: usize) -> std::io::Result<Option<String>> {
    let mut ht = HeadTail::new(cap);
    let mut stripper = AnsiStripper::new();
    let mut buf = [0u8; 16 * 1024];
    let mut clean = Vec::with_capacity(buf.len());
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        clean.clear();
        stripper.feed(&buf[..n], &mut clean);
        ht.push(&clean);
    }
    if ht.total() == 0 {
        return Ok(None);
    }
    let text = ht.finish();
    let text = text.trim_end().to_string();
    Ok((!text.trim().is_empty()).then_some(text))
}

// --------------------------------------------------------- shell facts ---

/// What the shell integration told us, before interpretation.
#[derive(Debug, Default, Clone)]
pub struct ShellInfo {
    pub shell: Option<String>,
    pub last_command: Option<String>,
    pub current_command: Option<String>,
    pub exit_code: Option<i32>,
}

/// The command of interest and whether its exit status is trustworthy.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandContext {
    pub command: String,
    pub exit_code: Option<i32>,
}

/// Decide which history entry matters (section 5.3).
///
/// - `producer | hey ...`: the producer is the command. Its status is not
///   observable from inside the pipeline, so it is dropped.
/// - bare `hey ...`: the previous entry is the command, with its status.
pub fn command_of_interest(info: &ShellInfo) -> Option<CommandContext> {
    if let Some(cur) = info.current_command.as_deref()
        && let Some(producer) = pipe_producer(cur)
    {
        return Some(CommandContext { command: producer, exit_code: None });
    }
    let last = info.last_command.as_deref()?.trim();
    if last.is_empty() {
        return None;
    }
    Some(CommandContext {
        command: last.to_string(),
        exit_code: info.exit_code,
    })
}

/// Text to the left of the last `| hey` (or `|& hey`, `| command hey`) in a line.
pub fn pipe_producer(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut found: Option<usize> = None;
    for (i, _) in line.match_indices('|') {
        // `||` is logical or, not a pipe.
        if bytes.get(i + 1) == Some(&b'|') || (i > 0 && bytes[i - 1] == b'|') {
            continue;
        }
        let mut rest = line[i + 1..].trim_start();
        rest = rest.strip_prefix('&').unwrap_or(rest).trim_start();
        rest = rest.strip_prefix("command ").map(str::trim_start).unwrap_or(rest);
        let word_end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>'))
            .unwrap_or(rest.len());
        if basename(&rest[..word_end]) == "hey" {
            found = Some(i);
        }
    }
    let left = line[..found?].trim();
    (!left.is_empty()).then(|| left.to_string())
}

// ------------------------------------------------------------- system ---

pub struct SystemInfo {
    pub lines: Vec<(&'static str, String)>,
}

pub fn detect_shell(flag: Option<&str>) -> Option<String> {
    if let Some(s) = flag {
        return Some(canonical_shell(basename(s)));
    }
    if let Ok(s) = std::env::var("SHELL")
        && !s.is_empty()
    {
        return Some(canonical_shell(basename(&s)));
    }
    cfg!(windows).then(|| "pwsh".to_string())
}

/// `powershell` and `pwsh` are one family for our purposes.
pub fn canonical_shell(name: &str) -> String {
    match name {
        "powershell" => "pwsh".to_string(),
        other => other.to_string(),
    }
}

pub fn system_info() -> SystemInfo {
    let mut lines = Vec::new();
    lines.push(("os", os_description()));
    if let Some(k) = kernel_release() {
        lines.push(("kernel", k));
    }
    lines.push(("arch", std::env::consts::ARCH.to_string()));
    SystemInfo { lines }
}

pub fn cwd_string() -> Option<String> {
    std::env::current_dir().ok().map(|p| p.display().to_string())
}

pub fn username() -> Option<String> {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

fn os_description() -> String {
    let mut desc = match std::env::consts::OS {
        "linux" => linux_distro().unwrap_or_else(|| "Linux".to_string()),
        "macos" => macos_version().unwrap_or_else(|| "macOS".to_string()),
        "windows" => "Windows".to_string(),
        other => other.to_string(),
    };
    if let Some(w) = wsl() {
        desc.push_str(&format!(" ({w})"));
    }
    desc
}

fn linux_distro() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    parse_os_release(&text)
}

pub fn parse_os_release(text: &str) -> Option<String> {
    let field = |k: &str| {
        text.lines().find_map(|l| {
            l.strip_prefix(k)
                .and_then(|r| r.strip_prefix('='))
                .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
                .filter(|v| !v.is_empty())
        })
    };
    match (field("NAME"), field("VERSION_ID")) {
        (Some(n), Some(v)) => Some(format!("{n} {v}")),
        _ => field("PRETTY_NAME").or_else(|| field("NAME")),
    }
}

fn macos_version() -> Option<String> {
    // Reading the plist is far cheaper than spawning `sw_vers`.
    let text = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let after_key = text.split("<key>ProductVersion</key>").nth(1)?;
    let ver = after_key.split("<string>").nth(1)?.split("</string>").next()?;
    Some(format!("macOS {}", ver.trim()))
}

fn kernel_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        return std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string());
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // SAFETY: uname fills a zeroed struct we own.
        unsafe {
            let mut u: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut u) == 0 {
                let c = std::ffi::CStr::from_ptr(u.release.as_ptr());
                return Some(c.to_string_lossy().into_owned());
            }
        }
        return None;
    }
    #[allow(unreachable_code)]
    None
}

fn wsl() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    wsl_from(&rel, std::env::var_os("WSL_DISTRO_NAME").is_some())
}

pub fn wsl_from(osrelease: &str, has_env: bool) -> Option<&'static str> {
    let l = osrelease.to_ascii_lowercase();
    if l.contains("wsl2") || (l.contains("microsoft") && l.contains("standard")) {
        Some("WSL2")
    } else if l.contains("microsoft") {
        Some("WSL1")
    } else if has_env {
        Some("WSL")
    } else {
        None
    }
}

// ---------------------------------------------------------------- git ---

#[derive(Debug, PartialEq, Eq, Default)]
pub struct GitInfo {
    pub branch: String,
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub remote: Option<String>,
}

impl GitInfo {
    pub fn summary(&self) -> String {
        let mut s = format!("branch {}", self.branch);
        let mut counts = Vec::new();
        for (n, label) in [
            (self.staged, "staged"),
            (self.modified, "modified"),
            (self.untracked, "untracked"),
            (self.conflicts, "conflicted"),
        ] {
            if n > 0 {
                counts.push(format!("{n} {label}"));
            }
        }
        if counts.is_empty() {
            s.push_str(", clean");
        } else {
            s.push_str(&format!(", {}", counts.join(", ")));
        }
        if let Some(r) = &self.remote {
            s.push_str(&format!(", remote {r}"));
        }
        s
    }
}

/// Branch, status counts and remote name. Everything shares one 200ms budget;
/// on timeout or failure (including "not a work tree") returns `None`.
pub fn git_info() -> Option<GitInfo> {
    let deadline = Instant::now() + Duration::from_millis(200);
    let left = || deadline.saturating_duration_since(Instant::now());

    let mut status = Command::new("git");
    status.args(["--no-optional-locks", "status", "--porcelain=v2", "--branch"]);
    let out = run_timeout(&mut status, left())?;
    if !out.success {
        return None;
    }
    let mut info = parse_git_status(&out.stdout);

    let mut remote = Command::new("git");
    remote.args(["remote"]);
    if let Some(o) = run_timeout(&mut remote, left())
        && o.success
    {
        info.remote = o.stdout.lines().next().map(|s| s.trim().to_string());
    }
    Some(info)
}

pub fn parse_git_status(text: &str) -> GitInfo {
    let mut g = GitInfo::default();
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            g.branch = if h == "(detached)" {
                match text.lines().find_map(|l| l.strip_prefix("# branch.oid ")) {
                    Some(oid) => format!("detached at {}", &oid[..oid.len().min(7)]),
                    None => "detached".to_string(),
                }
            } else {
                h.to_string()
            };
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // "1 XY ..." where X is the index state and Y the worktree state.
            let mut xy = line[2..].chars();
            if xy.next().is_some_and(|c| c != '.') {
                g.staged += 1;
            }
            if xy.next().is_some_and(|c| c != '.') {
                g.modified += 1;
            }
        } else if line.starts_with("u ") {
            g.conflicts += 1;
        } else if line.starts_with("? ") {
            g.untracked += 1;
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_osc_and_charset_sequences() {
        let s = "\x1b[1;31merror\x1b[0m: \x1b]0;title\x07ok \x1b]8;;http://x\x1b\\link\x1b(B done";
        assert_eq!(strip_ansi(s), "error: ok link done");
    }

    #[test]
    fn strips_across_chunk_boundaries() {
        let mut st = AnsiStripper::new();
        let mut out = Vec::new();
        st.feed(b"a\x1b[3", &mut out);
        st.feed(b"1mb\x1b", &mut out);
        st.feed(b"[0mc", &mut out);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn keeps_utf8_and_plain_text() {
        assert_eq!(strip_ansi("héllo → wörld\n"), "héllo → wörld\n");
    }

    #[test]
    fn small_input_is_untouched() {
        let got = read_capped("hello\nworld\n".as_bytes(), 1000).unwrap();
        assert_eq!(got.as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn empty_and_blank_input_is_none() {
        assert_eq!(read_capped(&b""[..], 100).unwrap(), None);
        assert_eq!(read_capped(&b"  \n\n"[..], 100).unwrap(), None);
    }

    #[test]
    fn truncates_keeping_40_head_60_tail() {
        let input: String = (0..1000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let got = read_capped(input.as_bytes(), 100).unwrap().unwrap();
        let (head, rest) = got.split_once("\n[... ").unwrap();
        let (marker, tail) = rest.split_once(" bytes truncated ...]\n").unwrap();
        assert_eq!(head.len(), 40);
        assert_eq!(tail.len(), 60);
        assert_eq!(marker.parse::<usize>().unwrap(), 900);
        assert!(input.starts_with(head));
        assert!(input.ends_with(tail));
    }

    #[test]
    fn truncation_holds_across_many_small_reads() {
        struct Drip(Vec<u8>, usize);
        impl Read for Drip {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let n = 7.min(b.len()).min(self.0.len() - self.1);
                b[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let data: Vec<u8> = (0..5000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let got = read_capped(Drip(data.clone(), 0), 200).unwrap().unwrap();
        assert!(got.contains("[... 4800 bytes truncated ...]"), "{got}");
        assert!(got.ends_with(std::str::from_utf8(&data[data.len() - 120..]).unwrap()));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let input = "é".repeat(500);
        let got = read_capped(input.as_bytes(), 101).unwrap().unwrap();
        assert!(!got.contains('\u{fffd}'), "{got}");
    }

    #[test]
    fn ansi_bytes_do_not_count_against_the_cap() {
        let input = "\x1b[31mx\x1b[0m".repeat(50);
        let got = read_capped(input.as_bytes(), 100).unwrap().unwrap();
        assert_eq!(got, "x".repeat(50));
    }

    #[test]
    fn pipe_producer_detection() {
        assert_eq!(pipe_producer("make 2>&1 | hey why"), Some("make 2>&1".into()));
        assert_eq!(pipe_producer("a | b | hey"), Some("a | b".into()));
        assert_eq!(pipe_producer("make |& hey"), Some("make".into()));
        assert_eq!(pipe_producer("make | command hey fix"), Some("make".into()));
        assert_eq!(pipe_producer("make | /usr/local/bin/hey"), Some("make".into()));
        assert_eq!(pipe_producer("make | hey | tee out"), Some("make".into()));
        assert_eq!(pipe_producer("hey why"), None);
        assert_eq!(pipe_producer("a || hey why"), None);
        assert_eq!(pipe_producer("make | heyyou"), None);
        assert_eq!(pipe_producer("make | grep hey"), None);
    }

    #[test]
    fn bare_mode_uses_previous_entry_with_status() {
        let info = ShellInfo {
            last_command: Some("git push origin main".into()),
            current_command: Some("hey why".into()),
            exit_code: Some(128),
            ..Default::default()
        };
        assert_eq!(
            command_of_interest(&info),
            Some(CommandContext { command: "git push origin main".into(), exit_code: Some(128) })
        );
    }

    #[test]
    fn pipe_mode_uses_producer_and_omits_status() {
        let info = ShellInfo {
            last_command: Some("ls".into()),
            current_command: Some("cargo build 2>&1 | hey".into()),
            exit_code: Some(0),
            ..Default::default()
        };
        assert_eq!(
            command_of_interest(&info),
            Some(CommandContext { command: "cargo build 2>&1".into(), exit_code: None })
        );
    }

    #[test]
    fn missing_history_is_tolerated() {
        let info = ShellInfo { exit_code: Some(1), ..Default::default() };
        assert_eq!(command_of_interest(&info), None);
        let info = ShellInfo { last_command: Some("  ".into()), ..Default::default() };
        assert_eq!(command_of_interest(&info), None);
    }

    #[test]
    fn os_release_parsing() {
        let t = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n";
        assert_eq!(parse_os_release(t).as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(parse_os_release("PRETTY_NAME=Arch Linux\n").as_deref(), Some("Arch Linux"));
        assert_eq!(parse_os_release(""), None);
    }

    #[test]
    fn wsl_detection() {
        assert_eq!(wsl_from("6.6.36.3-microsoft-standard-WSL2", false), Some("WSL2"));
        assert_eq!(wsl_from("4.4.0-19041-Microsoft", false), Some("WSL1"));
        assert_eq!(wsl_from("6.8.0-generic", false), None);
    }

    #[test]
    fn git_status_parsing() {
        let t = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n\
                 1 M. N... 100644 100644 100644 a b f1\n\
                 1 .M N... 100644 100644 100644 a b f2\n\
                 1 MM N... 100644 100644 100644 a b f3\n\
                 2 R. N... 100644 100644 100644 a b R100 new\told\n\
                 u UU N... 1 2 3 4 a b c f4\n\
                 ? untracked.txt\n";
        let g = parse_git_status(t);
        assert_eq!(g.branch, "main");
        assert_eq!((g.staged, g.modified, g.untracked, g.conflicts), (3, 2, 1, 1));
        assert_eq!(
            g.summary(),
            "branch main, 3 staged, 2 modified, 1 untracked, 1 conflicted"
        );
    }

    #[test]
    fn git_detached_and_clean() {
        let g = parse_git_status("# branch.oid 0123456789\n# branch.head (detached)\n");
        assert_eq!(g.summary(), "branch detached at 0123456, clean");
    }

    #[test]
    fn shell_names() {
        assert_eq!(detect_shell(Some("/usr/bin/zsh")).as_deref(), Some("zsh"));
        assert_eq!(detect_shell(Some("powershell")).as_deref(), Some("pwsh"));
    }
}
