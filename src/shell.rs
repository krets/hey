//! Shell integration: snippet templates and profile management (spec section 5).

use crate::context::{canonical_shell, detect_shell};
use crate::error::{Error, Result, usage_err};
use crate::util::{display_path, home_dir, run_timeout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const SNIPPET_VERSION: u32 = 1;

const START_PREFIX: &str = "# >>> hey shell integration";
const END_MARKER: &str = "# <<< hey shell integration <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sh {
    Bash,
    Zsh,
    Pwsh,
}

impl Sh {
    pub fn parse(name: &str) -> Result<Sh> {
        match canonical_shell(crate::util::basename(name)).as_str() {
            "bash" => Ok(Sh::Bash),
            "zsh" => Ok(Sh::Zsh),
            "pwsh" => Ok(Sh::Pwsh),
            other => usage_err(format!(
                "unsupported shell '{other}' (supported: bash, zsh, pwsh). Use --shell to choose one"
            )),
        }
    }

    /// `--shell` wins; otherwise `$SHELL` (or PowerShell on Windows).
    pub fn resolve(flag: Option<&str>) -> Result<Sh> {
        match detect_shell(flag) {
            Some(name) => Sh::parse(&name),
            None => usage_err("cannot detect your shell. Use --shell <bash|zsh|pwsh>"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sh::Bash => "bash",
            Sh::Zsh => "zsh",
            Sh::Pwsh => "pwsh",
        }
    }

    fn body(self) -> &'static str {
        match self {
            Sh::Bash => include_str!("../snippets/bash.sh"),
            Sh::Zsh => include_str!("../snippets/zsh.sh"),
            Sh::Pwsh => include_str!("../snippets/pwsh.ps1"),
        }
    }

    pub fn profile_path(self) -> Result<PathBuf> {
        let home = || {
            home_dir().ok_or_else(|| Error::Config("cannot locate home directory".into()))
        };
        match self {
            Sh::Bash => Ok(home()?.join(".bashrc")),
            Sh::Zsh => {
                let dir = std::env::var_os("ZDOTDIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from);
                Ok(dir.unwrap_or(home()?).join(".zshrc"))
            }
            Sh::Pwsh => {
                if let Some(p) = pwsh_profile_from_shell() {
                    return Ok(p);
                }
                let h = home()?;
                Ok(if cfg!(windows) {
                    h.join("Documents").join("PowerShell").join("Microsoft.PowerShell_profile.ps1")
                } else {
                    h.join(".config").join("powershell").join("Microsoft.PowerShell_profile.ps1")
                })
            }
        }
    }
}

/// Ask PowerShell itself where `$PROFILE` (CurrentUserCurrentHost) is.
fn pwsh_profile_from_shell() -> Option<PathBuf> {
    for exe in ["pwsh", "powershell"] {
        let mut cmd = Command::new(exe);
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", "$PROFILE"]);
        if let Some(o) = run_timeout(&mut cmd, Duration::from_secs(5))
            && o.success
        {
            let p = o.stdout.trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

/// The marked block, ready to print or splice into a profile.
pub fn block(sh: Sh, eol: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("{START_PREFIX} v{SNIPPET_VERSION} >>>{eol}"));
    for line in sh.body().lines() {
        out.push_str(line);
        out.push_str(eol);
    }
    out.push_str(END_MARKER);
    out.push_str(eol);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    /// Line indices of the start and end markers, inclusive.
    pub start: usize,
    pub end: usize,
    pub version: Option<u32>,
}

pub fn find_block(text: &str) -> Result<Option<Found>> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(start) = lines.iter().position(|l| l.trim().starts_with(START_PREFIX)) else {
        return Ok(None);
    };
    let end = lines[start..]
        .iter()
        .position(|l| l.trim() == END_MARKER)
        .map(|i| i + start)
        .ok_or_else(|| {
            Error::Config(
                "found the start of a hey shell block but not its end marker; fix the profile by hand"
                    .into(),
            )
        })?;
    let version = lines[start]
        .trim()
        .strip_prefix(START_PREFIX)
        .and_then(|r| r.trim().strip_prefix('v'))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok());
    Ok(Some(Found { start, end, version }))
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Installed,
    Updated,
    Unchanged,
}

pub fn apply_install(text: &str, sh: Sh) -> Result<(String, Change)> {
    let eol = eol_of(text);
    let new_block = block(sh, eol);
    match find_block(text)? {
        Some(f) => {
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let old: String = lines[f.start..=f.end].concat();
            // A block whose final line lacks a newline compares equal modulo it.
            if old.trim_end() == new_block.trim_end() {
                return Ok((text.to_string(), Change::Unchanged));
            }
            let mut out: String = lines[..f.start].concat();
            out.push_str(&new_block);
            out.push_str(&lines[f.end + 1..].concat());
            Ok((out, Change::Updated))
        }
        None => {
            let mut out = text.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(eol);
                }
                out.push_str(eol);
            }
            out.push_str(&new_block);
            Ok((out, Change::Installed))
        }
    }
}

/// Remove the block and the one blank line `install` put in front of it.
pub fn apply_uninstall(text: &str) -> Result<Option<String>> {
    let Some(f) = find_block(text)? else {
        return Ok(None);
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut start = f.start;
    if start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let mut out: String = lines[..start].concat();
    out.push_str(&lines[f.end + 1..].concat());
    Ok(Some(out))
}

fn backup_path(profile: &Path) -> PathBuf {
    let mut name = profile.file_name().unwrap_or_default().to_os_string();
    name.push(".hey.bak");
    profile.with_file_name(name)
}

/// Copy the profile aside the first time we touch it.
fn backup_once(profile: &Path) -> Result<()> {
    let bak = backup_path(profile);
    if profile.exists() && !bak.exists() {
        std::fs::copy(profile, &bak).map_err(|e| {
            Error::Config(format!("cannot write backup {}: {e}", display_path(&bak)))
        })?;
    }
    Ok(())
}

fn read_profile(profile: &Path) -> Result<String> {
    match std::fs::read_to_string(profile) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(Error::Config(format!("cannot read {}: {e}", display_path(profile)))),
    }
}

fn write_profile(profile: &Path, text: &str) -> Result<()> {
    if let Some(dir) = profile.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Config(format!("cannot create {}: {e}", display_path(dir))))?;
    }
    std::fs::write(profile, text)
        .map_err(|e| Error::Config(format!("cannot write {}: {e}", display_path(profile))))
}

pub fn install(sh: Sh) -> Result<(PathBuf, Change)> {
    let profile = sh.profile_path()?;
    let text = read_profile(&profile)?;
    let (new_text, change) = apply_install(&text, sh)?;
    if change != Change::Unchanged {
        backup_once(&profile)?;
        write_profile(&profile, &new_text)?;
    }
    Ok((profile, change))
}

/// Returns the profile and whether a block was removed.
pub fn uninstall(sh: Sh) -> Result<(PathBuf, bool)> {
    let profile = sh.profile_path()?;
    let text = read_profile(&profile)?;
    match apply_uninstall(&text)? {
        Some(new_text) => {
            backup_once(&profile)?;
            write_profile(&profile, &new_text)?;
            Ok((profile, true))
        }
        None => Ok((profile, false)),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Absent,
    Current,
    Stale(Option<u32>),
}

pub struct Status {
    pub profile: PathBuf,
    pub state: State,
}

pub fn status(sh: Sh) -> Result<Status> {
    let profile = sh.profile_path()?;
    let text = read_profile(&profile)?;
    let state = match find_block(&text)? {
        None => State::Absent,
        Some(f) if f.version == Some(SNIPPET_VERSION) => State::Current,
        Some(f) => State::Stale(f.version),
    };
    Ok(Status { profile, state })
}

impl Status {
    pub fn describe(&self) -> String {
        match &self.state {
            State::Absent => "not installed".to_string(),
            State::Current => format!("installed, snippet v{SNIPPET_VERSION} (current)"),
            State::Stale(Some(v)) => format!(
                "installed, snippet v{v} is out of date (current is v{SNIPPET_VERSION}). Run: hey shell install"
            ),
            State::Stale(None) => "installed, snippet version unknown. Run: hey shell install".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_marked_and_versioned() {
        let b = block(Sh::Bash, "\n");
        assert!(b.starts_with("# >>> hey shell integration v1 >>>\n"));
        assert!(b.ends_with("# <<< hey shell integration <<<\n"));
        assert!(b.contains("command hey --shell bash"));
    }

    #[test]
    fn every_snippet_meets_the_function_contract() {
        for sh in [Sh::Bash, Sh::Zsh] {
            let body = sh.body();
            let first_stmt = body.lines().find(|l| l.trim_start().starts_with("local __hey_rc")).unwrap();
            assert!(first_stmt.contains("=$?"));
            let pos_rc = body.find("__hey_rc=$?").unwrap();
            // status capture must come before any history lookup
            let pos_hist = body.find("HISTCMD").or_else(|| body.find("fc -ln")).unwrap();
            assert!(pos_rc < pos_hist);
            for flag in ["--shell", "--last-command", "--exit-code", "\"$@\""] {
                assert!(body.contains(flag), "{flag} missing from {sh:?}");
            }
        }
        let ps = Sh::Pwsh.body();
        assert!(ps.contains("$?") && ps.contains("LASTEXITCODE") && ps.contains("Get-History -Count 1"));
        assert!(ps.contains("-CommandType Application"));
    }

    #[test]
    fn install_appends_to_empty_and_existing_profiles() {
        let (out, ch) = apply_install("", Sh::Bash).unwrap();
        assert_eq!(ch, Change::Installed);
        assert!(out.starts_with("# >>> hey"));

        let (out, ch) = apply_install("export A=1", Sh::Bash).unwrap();
        assert_eq!(ch, Change::Installed);
        assert!(out.starts_with("export A=1\n\n# >>> hey shell integration v1 >>>\n"), "{out}");
    }

    #[test]
    fn install_is_idempotent() {
        let (once, _) = apply_install("export A=1\n", Sh::Zsh).unwrap();
        let (twice, ch) = apply_install(&once, Sh::Zsh).unwrap();
        assert_eq!(ch, Change::Unchanged);
        assert_eq!(once, twice);
        assert_eq!(once.matches(">>> hey shell integration").count(), 1);
    }

    #[test]
    fn install_replaces_a_stale_block_in_place() {
        let stale = "before\n# >>> hey shell integration v0 >>>\nold stuff\n# <<< hey shell integration <<<\nafter\n";
        let (out, ch) = apply_install(stale, Sh::Bash).unwrap();
        assert_eq!(ch, Change::Updated);
        assert!(out.starts_with("before\n# >>> hey shell integration v1 >>>\n"));
        assert!(out.ends_with("# <<< hey shell integration <<<\nafter\n"));
        assert!(!out.contains("old stuff"));
    }

    #[test]
    fn uninstall_restores_the_original() {
        let original = "export A=1\nalias x=y\n";
        let (installed, _) = apply_install(original, Sh::Bash).unwrap();
        assert_eq!(apply_uninstall(&installed).unwrap().as_deref(), Some(original));
        assert_eq!(apply_uninstall(original).unwrap(), None);
    }

    #[test]
    fn uninstall_keeps_surrounding_content() {
        let text = "a\n\n# >>> hey shell integration v1 >>>\nx\n# <<< hey shell integration <<<\nb\n";
        assert_eq!(apply_uninstall(text).unwrap().as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn crlf_profiles_stay_crlf() {
        let (out, _) = apply_install("a\r\n", Sh::Pwsh).unwrap();
        assert!(out.lines().count() > 3);
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn unterminated_block_is_an_error_not_a_duplicate() {
        let text = "# >>> hey shell integration v1 >>>\nbroken\n";
        assert!(apply_install(text, Sh::Bash).is_err());
        assert!(apply_uninstall(text).is_err());
    }

    #[test]
    fn versions_are_parsed() {
        let f = find_block("# >>> hey shell integration v7 >>>\n# <<< hey shell integration <<<\n")
            .unwrap()
            .unwrap();
        assert_eq!((f.start, f.end, f.version), (0, 1, Some(7)));
        assert_eq!(find_block("nothing here\n").unwrap(), None);
    }

    #[test]
    fn shell_names() {
        assert_eq!(Sh::parse("/bin/zsh").unwrap(), Sh::Zsh);
        assert_eq!(Sh::parse("powershell").unwrap(), Sh::Pwsh);
        assert!(Sh::parse("fish").is_err());
    }

    #[test]
    fn backup_name() {
        assert_eq!(backup_path(Path::new("/h/.bashrc")), PathBuf::from("/h/.bashrc.hey.bak"));
    }
}
