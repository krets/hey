//! Layered configuration: env > local `./.hey` > global `~/.hey` > defaults.

use crate::error::{Error, Result, config_err};
use crate::ini::{Ini, KeyPath, flatten};
use crate::util::{self, display_path, home_dir, run_timeout, shell_command};
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a command line assistant. The user is in a terminal. Answer as concisely as possible. Output must be accurate, correct, and scoped to the question. Prefer a single corrected command when one exists. No markdown, no preamble.";
pub const DEFAULT_QUESTION: &str =
    "Explain this output and, if it indicates an error, give the fix.";

pub const BUILTIN_SUBCOMMANDS: &[&str] = &["config", "shell", "doctor", "version", "help"];

/// Built-in defaults, consulted last.
pub fn default_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "core.max_tokens" => "800",
        "core.timeout" => "30",
        "core.stream" => "false",
        "core.color" => "false",
        "context.system" => "true",
        "context.git" => "false",
        "context.max_stdin_bytes" => "32768",
        "prompt.system" => DEFAULT_SYSTEM_PROMPT,
        "prompt.default_question" => DEFAULT_QUESTION,
        _ => return None,
    })
}

/// Check a value against the keys that have a fixed type.
pub fn validate(key: &str, value: &str) -> std::result::Result<(), String> {
    let kp = KeyPath::parse(key).ok_or_else(|| format!("invalid key: {key}"))?;
    let dotted = kp.dotted();
    match dotted.as_str() {
        "core.max_tokens" | "core.timeout" | "context.max_stdin_bytes" => {
            match value.trim().parse::<u64>() {
                Ok(n) if n > 0 => Ok(()),
                _ => Err(format!("{dotted} must be a positive integer")),
            }
        }
        "core.stream" | "core.color" | "context.system" | "context.git" => {
            util::is_truthy(value)
                .map(|_| ())
                .ok_or_else(|| format!("{dotted} must be true or false"))
        }
        _ if kp.section == "provider" && kp.sub.is_some() && kp.name == "type" => {
            match value {
                "anthropic" | "openai" | "gemini" => Ok(()),
                _ => Err("provider type must be one of: anthropic, openai, gemini".into()),
            }
        }
        _ if kp.section == "alias" && kp.sub.is_some() => {
            let name = kp.sub.as_deref().unwrap_or("");
            if BUILTIN_SUBCOMMANDS.contains(&name) {
                return Err(format!("'{name}' is a builtin command and cannot be an alias"));
            }
            if kp.name == "no_context" && util::is_truthy(value).is_none() {
                return Err("alias no_context must be true or false".into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Local,
    Global,
}

impl ScopeKind {
    pub fn name(self) -> &'static str {
        match self {
            ScopeKind::Local => "local",
            ScopeKind::Global => "global",
        }
    }
}

#[derive(Debug)]
pub struct Scope {
    pub kind: ScopeKind,
    pub path: PathBuf,
    pub exists: bool,
    pub ini: Ini,
}

impl Scope {
    /// Load a scope. A missing file is an empty scope, not an error.
    pub fn load(kind: ScopeKind, path: PathBuf) -> std::result::Result<Scope, String> {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let ini = Ini::parse(&text)
                    .map_err(|e| format!("{}:{}", display_path(&path), e))?;
                Ok(Scope { kind, path, exists: true, ini })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Scope {
                kind,
                path,
                exists: false,
                ini: Ini::default(),
            }),
            Err(e) => Err(format!("cannot read {}: {e}", display_path(&path))),
        }
    }

    /// Write the scope, leaving the file at 0600.
    pub fn save(&mut self) -> Result<()> {
        write_private(&self.path, &self.ini.render())
            .map_err(|e| Error::Config(format!("cannot write {}: {e}", display_path(&self.path))))?;
        self.exists = true;
        Ok(())
    }

    /// Does this file define any credential-bearing field?
    pub fn has_secret_fields(&self) -> bool {
        self.ini.entries().iter().any(|e| is_secret_key(&e.key))
    }
}

/// `provider.<name>.key` and `provider.<name>.key_cmd`. Both hand out
/// credentials (`key_cmd` also runs a command), so both are guarded.
pub fn is_secret_key(dotted: &str) -> bool {
    dotted.starts_with("provider.") && (dotted.ends_with(".key") || dotted.ends_with(".key_cmd"))
}

#[cfg(unix)]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Group/other bits set on a config file, if any (Unix only).
#[cfg(unix)]
pub fn insecure_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then_some(mode & 0o777)
}

#[cfg(not(unix))]
pub fn insecure_mode(_path: &Path) -> Option<u32> {
    None
}

/// Windows best effort: names of principals other than the owner, SYSTEM and
/// Administrators that are granted access. Empty elsewhere or on any failure.
#[cfg(windows)]
pub fn foreign_acl_principals(path: &Path) -> Vec<String> {
    let mut cmd = Command::new("icacls");
    cmd.arg(path);
    let Some(out) = run_timeout(&mut cmd, Duration::from_secs(2)) else {
        return Vec::new();
    };
    let me = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    let mut found = Vec::new();
    for line in out.stdout.lines() {
        // Entries look like `C:\path NT AUTHORITY\SYSTEM:(F)` or `   BUILTIN\Users:(RX)`.
        let Some(idx) = line.rfind(":(") else { continue };
        let head = line[..idx].trim();
        let principal = match head.find(char::is_whitespace) {
            Some(_) if head.contains(":\\") => head.rsplit(' ').next().unwrap_or(head),
            _ => head,
        };
        let p = principal.to_lowercase();
        let ok = p.ends_with("\\system")
            || p.ends_with("\\administrators")
            || p.ends_with(&format!("\\{me}"))
            || p == me;
        if !ok {
            found.push(principal.to_string());
        }
    }
    found
}

#[cfg(not(windows))]
pub fn foreign_acl_principals(_path: &Path) -> Vec<String> {
    Vec::new()
}

pub fn insecure_message(path: &Path, mode: u32) -> String {
    let p = display_path(path);
    format!("{p} is readable by others (mode {mode:04o}). Run: chmod 600 {p}  or  hey doctor --fix")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalGuard {
    /// No local file, or it holds no credential fields.
    Clear,
    /// Has credential fields, but the tree is not a git work tree or the file is ignored.
    KeysWarn,
    /// Has credential fields inside an unignored git work tree: not used.
    KeysBlocked,
}

pub struct Config {
    pub global: Scope,
    pub local: Option<Scope>,
    guard: OnceCell<LocalGuard>,
}

pub fn global_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HEY_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    home_dir().map(|h| h.join(".hey"))
}

pub fn local_path() -> PathBuf {
    PathBuf::from(".hey")
}

impl Config {
    pub fn from_scopes(global: Scope, local: Option<Scope>) -> Config {
        Config { global, local, guard: OnceCell::new() }
    }

    pub fn load() -> Result<Config> {
        let gpath = global_path()
            .ok_or_else(|| Error::Config("cannot locate home directory; set HEY_CONFIG".into()))?;
        let global = Scope::load(ScopeKind::Global, gpath).map_err(Error::Config)?;
        let local = Self::load_local(&global.path).map_err(Error::Config)?;
        Ok(Config::from_scopes(global, local))
    }

    /// The local scope, unless it is the very same file as the global one
    /// (running from `$HOME`).
    pub fn load_local(global: &Path) -> std::result::Result<Option<Scope>, String> {
        let lpath = local_path();
        if !lpath.exists() {
            return Ok(None);
        }
        if let (Ok(a), Ok(b)) = (lpath.canonicalize(), global.canonicalize())
            && a == b
        {
            return Ok(None);
        }
        Scope::load(ScopeKind::Local, lpath).map(Some)
    }

    pub fn scopes(&self) -> Vec<&Scope> {
        let mut v = vec![&self.global];
        v.extend(self.local.as_ref());
        v
    }

    /// Refuse to proceed when the global file (or a local file holding keys)
    /// is readable by others.
    pub fn check_permissions(&self) -> Result<()> {
        for s in self.scopes() {
            if !s.exists {
                continue;
            }
            if s.kind == ScopeKind::Local && !s.has_secret_fields() {
                continue;
            }
            if let Some(mode) = insecure_mode(&s.path) {
                return Err(Error::Refused(insecure_message(&s.path, mode)));
            }
            if cfg!(windows) {
                let who = foreign_acl_principals(&s.path);
                if !who.is_empty() {
                    eprintln!(
                        "hey: warning: {} grants access to {}",
                        display_path(&s.path),
                        who.join(", ")
                    );
                }
            }
        }
        Ok(())
    }

    pub fn local_guard(&self) -> LocalGuard {
        *self.guard.get_or_init(|| {
            let Some(local) = &self.local else { return LocalGuard::Clear };
            if !local.has_secret_fields() {
                return LocalGuard::Clear;
            }
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if !util::inside_git_tree(&cwd) {
                return LocalGuard::KeysWarn;
            }
            // `git check-ignore -q` exits 0 only when the path is ignored.
            let mut cmd = Command::new("git");
            cmd.args(["check-ignore", "-q", ".hey"]);
            match run_timeout(&mut cmd, Duration::from_secs(2)) {
                Some(o) if o.success => LocalGuard::KeysWarn,
                _ => LocalGuard::KeysBlocked,
            }
        })
    }

    /// Print the section 4.2 warning, if it applies.
    pub fn warn_local_keys(&self) {
        match self.local_guard() {
            LocalGuard::Clear => {}
            LocalGuard::KeysWarn => eprintln!(
                "hey: warning: ./.hey contains provider credentials; keep keys in {} instead",
                global_display(self)
            ),
            LocalGuard::KeysBlocked => eprintln!(
                "hey: ./.hey contains provider credentials and is inside a git work tree without being ignored; \
                 ignoring those keys. Add .hey to .gitignore or move them to {}",
                global_display(self)
            ),
        }
    }

    fn raw_get(&self, dotted: &str, allow_local_secrets: bool) -> Option<String> {
        if let Some(local) = &self.local
            && (allow_local_secrets || !is_secret_key(dotted))
            && let Some(v) = local.ini.get(dotted)
        {
            return Some(v);
        }
        self.global.ini.get(dotted)
    }

    /// Value for `key` across scopes, then built-in defaults.
    /// Credential fields are never taken from a blocked local file.
    pub fn get(&self, key: &str) -> Option<String> {
        let dotted = KeyPath::parse(key)?.dotted();
        let allow = self.local_guard() != LocalGuard::KeysBlocked;
        self.raw_get(&dotted, allow)
            .or_else(|| default_for(&dotted).map(String::from))
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key)
            .and_then(|v| util::is_truthy(&v))
            .unwrap_or(false)
    }

    pub fn get_u64(&self, key: &str, fallback: u64) -> u64 {
        self.get(key)
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(fallback)
    }

    /// `provider.<name>.<field>`
    pub fn provider_field(&self, name: &str, field: &str) -> Option<String> {
        self.get(&flatten("provider", Some(name), field))
    }

    /// Names of every configured `[provider "x"]`, in first-seen order.
    pub fn provider_names(&self) -> Vec<String> {
        self.sub_names("provider")
    }

    pub fn alias_names(&self) -> Vec<String> {
        self.sub_names("alias")
    }

    fn sub_names(&self, section: &str) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for s in self.scopes() {
            for e in s.ini.entries() {
                if let Some(rest) = e.key.strip_prefix(section).and_then(|r| r.strip_prefix('.'))
                    && let Some(idx) = rest.rfind('.')
                {
                    let name = rest[..idx].to_string();
                    if !seen.contains(&name) {
                        seen.push(name);
                    }
                }
            }
        }
        seen
    }

    pub fn alias_field(&self, name: &str, field: &str) -> Option<String> {
        self.get(&flatten("alias", Some(name), field))
    }

    /// Does any credential source exist for this provider (without running it)?
    pub fn has_key_source(&self, name: &str) -> bool {
        self.has_key_source_inner(name, &mut Vec::new())
    }

    fn has_key_source_inner(&self, name: &str, seen: &mut Vec<String>) -> bool {
        if seen.iter().any(|s| s == name) {
            return false;
        }
        seen.push(name.to_string());
        if env_key(name).is_some()
            || self.provider_field(name, "key_cmd").is_some()
            || self.provider_field(name, "key").is_some()
        {
            return true;
        }
        match self.provider_field(name, "key_from") {
            Some(other) => self.has_key_source_inner(&other, seen),
            None => false,
        }
    }

    /// Resolve the API key: env, `key_cmd`, `key`, then `key_from`.
    /// `Ok(None)` means the provider has no credentials configured.
    pub fn resolve_key(&self, name: &str) -> Result<Option<String>> {
        self.resolve_key_inner(name, &mut Vec::new())
    }

    fn resolve_key_inner(&self, name: &str, seen: &mut Vec<String>) -> Result<Option<String>> {
        if seen.iter().any(|s| s == name) {
            return config_err(format!("key_from cycle through provider '{name}'"));
        }
        seen.push(name.to_string());

        if let Some(k) = env_key(name) {
            return Ok(Some(k));
        }
        if let Some(cmdline) = self.provider_field(name, "key_cmd") {
            return run_key_cmd(name, &cmdline).map(Some);
        }
        if let Some(k) = self.provider_field(name, "key") {
            let k = k.trim().to_string();
            if !k.is_empty() {
                return Ok(Some(k));
            }
        }
        if let Some(other) = self.provider_field(name, "key_from") {
            return self.resolve_key_inner(&other, seen);
        }
        Ok(None)
    }
}

fn global_display(cfg: &Config) -> String {
    display_path(&cfg.global.path)
}

pub fn env_key_name(provider: &str) -> String {
    let up: String = provider
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("HEY_{up}_KEY")
}

pub fn env_key(provider: &str) -> Option<String> {
    std::env::var(env_key_name(provider))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn run_key_cmd(provider: &str, line: &str) -> Result<String> {
    // Generous timeout: the command may need to unlock a password store.
    let mut cmd = shell_command(line);
    let out = run_timeout(&mut cmd, Duration::from_secs(60));
    match out {
        Some(o) if o.success && !o.stdout.trim().is_empty() => Ok(o.stdout.trim().to_string()),
        Some(o) if o.success => {
            config_err(format!("key_cmd for provider '{provider}' printed nothing"))
        }
        _ => config_err(format!("key_cmd for provider '{provider}' failed")),
    }
}

/// `sk-ant-abcdef1234` becomes `sk-a...1234`; short values are fully masked.
pub fn redact(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(global: &str, local: Option<&str>) -> Config {
        cfg_guard(global, local, LocalGuard::KeysWarn)
    }

    fn cfg_guard(global: &str, local: Option<&str>, guard: LocalGuard) -> Config {
        Config {
            global: Scope {
                kind: ScopeKind::Global,
                path: PathBuf::from("/nonexistent/.hey"),
                exists: true,
                ini: Ini::parse(global).unwrap(),
            },
            local: local.map(|t| Scope {
                kind: ScopeKind::Local,
                path: PathBuf::from(".hey"),
                exists: true,
                ini: Ini::parse(t).unwrap(),
            }),
            guard: {
                // Tests must not shell out to git.
                let c = OnceCell::new();
                let _ = c.set(guard);
                c
            },
        }
    }

    #[test]
    fn local_overrides_global_per_key() {
        let c = cfg(
            "[core]\n provider = a\n max_tokens = 100\n",
            Some("[core]\n provider = b\n"),
        );
        assert_eq!(c.get("core.provider").as_deref(), Some("b"));
        assert_eq!(c.get("core.max_tokens").as_deref(), Some("100"));
    }

    #[test]
    fn defaults_apply_last() {
        let c = cfg("", None);
        assert_eq!(c.get("core.timeout").as_deref(), Some("30"));
        assert_eq!(c.get("core.nope"), None);
        assert_eq!(c.get_u64("core.max_tokens", 1), 800);
    }

    #[test]
    fn blocked_local_keys_are_ignored() {
        let global = "[provider \"p\"]\n key = global\n";
        let local = "[provider \"p\"]\n key = local\n model = m\n";
        let open = cfg_guard(global, Some(local), LocalGuard::KeysWarn);
        assert_eq!(open.provider_field("p", "key").as_deref(), Some("local"));
        let blocked = cfg_guard(global, Some(local), LocalGuard::KeysBlocked);
        assert_eq!(blocked.provider_field("p", "key").as_deref(), Some("global"));
        assert_eq!(blocked.provider_field("p", "model").as_deref(), Some("m"));
    }

    #[test]
    fn key_resolution_order_and_key_from() {
        let c = cfg(
            "[provider \"a\"]\n key = KEYA\n[provider \"b\"]\n key_from = a\n[provider \"c\"]\n type = openai\n",
            None,
        );
        assert_eq!(c.resolve_key("a").unwrap().as_deref(), Some("KEYA"));
        assert_eq!(c.resolve_key("b").unwrap().as_deref(), Some("KEYA"));
        assert_eq!(c.resolve_key("c").unwrap(), None);
        assert!(c.has_key_source("b"));
        assert!(!c.has_key_source("c"));
    }

    #[test]
    fn key_cmd_beats_key() {
        let c = cfg("[provider \"a\"]\n key = plain\n key_cmd = echo fromcmd\n", None);
        if cfg!(unix) {
            assert_eq!(c.resolve_key("a").unwrap().as_deref(), Some("fromcmd"));
        }
    }

    #[test]
    fn key_from_cycle_is_an_error() {
        let c = cfg("[provider \"a\"]\n key_from = b\n[provider \"b\"]\n key_from = a\n", None);
        assert!(matches!(c.resolve_key("a"), Err(Error::Config(_))));
        assert!(!c.has_key_source("a"));
    }

    #[test]
    fn env_key_name_mapping() {
        assert_eq!(env_key_name("anthropic-big"), "HEY_ANTHROPIC_BIG_KEY");
    }

    #[test]
    fn provider_and_alias_names() {
        let c = cfg(
            "[provider \"a\"]\n type=openai\n[alias \"fix\"]\n prompt=x\n",
            Some("[provider \"b\"]\n model=m\n"),
        );
        assert_eq!(c.provider_names(), vec!["a", "b"]);
        assert_eq!(c.alias_names(), vec!["fix"]);
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("sk-ant-api03-abcdefwxyz"), "sk-a...wxyz");
        assert_eq!(redact("short"), "****");
    }

    #[test]
    fn validation() {
        assert!(validate("core.max_tokens", "abc").is_err());
        assert!(validate("core.max_tokens", "500").is_ok());
        assert!(validate("core.stream", "maybe").is_err());
        assert!(validate("provider.x.type", "cohere").is_err());
        assert!(validate("alias.config.prompt", "x").is_err());
        assert!(validate("alias.fix.prompt", "x").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn insecure_mode_detects_group_and_other_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hey-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("c");
        std::fs::write(&f, "").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(insecure_mode(&f), Some(0o644));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(insecure_mode(&f), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
