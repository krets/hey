//! `hey doctor` (spec section 10): one line per check.

use crate::config::{
    self, Config, LocalGuard, Scope, ScopeKind, foreign_acl_principals,
    insecure_message, insecure_mode,
};
use crate::error::{Error, Result, usage_err};
use crate::provider::{self, Params};
use crate::shell::{self, Sh, State};
use crate::util::display_path;
use std::time::Instant;

struct Report {
    code: u8,
}

/// Exit code priority: refused (4) beats config (3) beats provider (1).
fn worse(a: u8, b: u8) -> u8 {
    let rank = |c: u8| match c {
        4 => 3,
        3 => 2,
        1 => 1,
        _ => 0,
    };
    if rank(b) > rank(a) { b } else { a }
}

impl Report {
    fn ok(&self, msg: impl AsRef<str>) {
        outln!("ok    {}", msg.as_ref());
    }
    fn info(&self, msg: impl AsRef<str>) {
        outln!("info  {}", msg.as_ref());
    }
    fn warn(&self, msg: impl AsRef<str>) {
        outln!("warn  {}", msg.as_ref());
    }
    fn fail(&mut self, code: u8, msg: impl AsRef<str>) {
        outln!("FAIL  {}", msg.as_ref());
        self.code = worse(self.code, code);
    }
}

pub fn run(args: Vec<String>) -> Result<()> {
    let mut fix = false;
    let mut ping = false;
    for a in &args {
        match a.as_str() {
            "--fix" => fix = true,
            "--ping" => ping = true,
            other => return usage_err(format!("unknown doctor option '{other}' (expected --fix, --ping)")),
        }
    }

    let mut rep = Report { code: 0 };
    let gpath = config::global_path();

    // 1. Files and parse errors.
    let mut loaded: Vec<Scope> = Vec::new();
    let mut parse_failed = false;
    let mut candidates: Vec<(ScopeKind, std::path::PathBuf)> = Vec::new();
    if let Some(g) = &gpath {
        candidates.push((ScopeKind::Global, g.clone()));
    } else {
        rep.fail(3, "config: cannot locate a home directory; set HEY_CONFIG");
    }
    match Config::load_local(gpath.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(Some(l)) => candidates.push((ScopeKind::Local, l.path)),
        Ok(None) => {}
        Err(e) => {
            parse_failed = true;
            rep.fail(3, format!("config: local: {e}"));
        }
    }
    for (kind, path) in candidates {
        match Scope::load(kind, path.clone()) {
            Ok(s) if s.exists => {
                rep.ok(format!(
                    "config: {} {} ({} entries)",
                    kind.name(),
                    display_path(&s.path),
                    s.ini.entries().len()
                ));
                loaded.push(s);
            }
            Ok(s) => {
                rep.info(format!("config: {} {} not found", kind.name(), display_path(&s.path)));
                loaded.push(s);
            }
            Err(e) => {
                parse_failed = true;
                rep.fail(3, format!("config: {} {e}", kind.name()));
            }
        }
    }

    // 2. Permissions.
    for s in loaded.iter().filter(|s| s.exists) {
        let p = display_path(&s.path);
        match insecure_mode(&s.path) {
            Some(mode) if s.kind == ScopeKind::Local && !s.has_secret_fields() => {
                rep.ok(format!("permissions: {p} is mode {mode:04o}; fine, it holds no keys"));
            }
            Some(mode) if fix => match set_private(&s.path) {
                Ok(()) => rep.ok(format!("permissions: {p} was mode {mode:04o}, now 0600")),
                Err(e) => rep.fail(4, format!("permissions: cannot fix {p}: {e}")),
            },
            Some(mode) => rep.fail(4, format!("permissions: {}", insecure_message(&s.path, mode))),
            None => {
                let who = foreign_acl_principals(&s.path);
                if who.is_empty() {
                    rep.ok(format!("permissions: {p}"));
                } else {
                    rep.warn(format!("permissions: {p} grants access to {}", who.join(", ")));
                }
            }
        }
    }

    // 3. Provider, model, key.
    let mut active: Option<provider::Provider> = None;
    let mut cfg: Option<Config> = None;
    if !parse_failed {
        let mut it = loaded.into_iter();
        if let Some(global) = it.next() {
            cfg = Some(Config::from_scopes(global, it.next()));
        }
    } else {
        rep.info("provider checks skipped until the config parses");
    }
    if let Some(cfg) = &cfg {
        let model_env = std::env::var("HEY_MODEL").ok().filter(|v| !v.is_empty());
        let resolved = provider::default_name(cfg)
            .and_then(|name| provider::resolve(cfg, &name, model_env.as_deref()));
        match resolved {
            Ok(p) => {
                rep.ok(format!(
                    "provider: {} (type {}, model {}, {})",
                    p.name, p.kind.name(), p.model, p.url
                ));
                check_key(&mut rep, cfg, &p);
                active = Some(p);
            }
            Err(e) => rep.fail(3, format!("provider: {e}")),
        }
        if let Some(esc) = cfg.get("core.escalate") {
            match provider::resolve(cfg, &esc, None) {
                Ok(p) => {
                    rep.ok(format!("escalate: {} (type {}, model {})", p.name, p.kind.name(), p.model));
                    if p.name != active.as_ref().map(|a| a.name.clone()).unwrap_or_default() {
                        check_key(&mut rep, cfg, &p);
                    }
                }
                Err(e) => rep.fail(3, format!("escalate: {e}")),
            }
        }

        // 4. Local config with keys.
        match cfg.local_guard() {
            LocalGuard::Clear => {
                if cfg.local.is_some() {
                    rep.ok("local config: holds no keys");
                }
            }
            LocalGuard::KeysWarn => rep.warn(
                "local config: ./.hey contains keys (outside a git work tree, or ignored); keep keys in the global file",
            ),
            LocalGuard::KeysBlocked => rep.fail(
                3,
                "local config: ./.hey contains keys inside an unignored git work tree; hey refuses to use them. Add .hey to .gitignore or move the keys",
            ),
        }
    }

    // 5. Shell integration. Absent is not an error.
    match Sh::resolve(None) {
        Ok(sh) => match shell::status(sh) {
            Ok(st) => match st.state {
                State::Absent => rep.info(format!(
                    "shell: {} integration not installed (optional; adds the previous command and exit status). Run: hey shell install",
                    sh.name()
                )),
                State::Current => rep.ok(format!("shell: {} integration in {} is current (v{})", sh.name(), display_path(&st.profile), shell::SNIPPET_VERSION)),
                State::Stale(_) => rep.warn(format!("shell: {} {}", sh.name(), st.describe())),
            },
            Err(e) => rep.warn(format!("shell: cannot inspect {} profile: {e}", sh.name())),
        },
        Err(_) => rep.info("shell: no supported shell detected (bash, zsh, pwsh); integration is optional"),
    }

    // 6. Optional live check.
    if ping {
        match (&cfg, &active) {
            (Some(cfg), Some(p)) => ping_provider(&mut rep, cfg, p),
            _ => rep.fail(3, "ping: no usable provider to ping"),
        }
    }

    if rep.code == 0 { Ok(()) } else { Err(Error::Silent(rep.code)) }
}

fn check_key(rep: &mut Report, cfg: &Config, p: &provider::Provider) {
    if !cfg.has_key_source(&p.name) {
        if p.url == p.kind.default_url() {
            rep.fail(
                3,
                format!(
                    "key: none for '{}'. Run: hey config init  (or: hey config set provider.{}.key)",
                    p.name,
                    p.name
                ),
            );
        } else {
            rep.info(format!("key: none configured for '{}' (fine for local endpoints)", p.name));
        }
        return;
    }
    // Never print the key itself.
    match cfg.resolve_key(&p.name) {
        Ok(Some(_)) => rep.ok(format!("key: resolvable for '{}'", p.name)),
        Ok(None) => rep.fail(3, format!("key: '{}' has a key source but it is empty or blocked", p.name)),
        Err(e) => rep.fail(3, format!("key: {e}")),
    }
}

fn ping_provider(rep: &mut Report, cfg: &Config, p: &provider::Provider) {
    let key = match cfg.resolve_key(&p.name) {
        Ok(k) => k,
        Err(e) => return rep.fail(3, format!("ping: {e}")),
    };
    let params = Params {
        system: "Reply with the single word: ok",
        user: "ping",
        max_tokens: 16,
        stream: false,
    };
    let req = provider::build_request(p, key.as_deref(), &params);
    let started = Instant::now();
    match provider::send(p.kind, &req, cfg.get_u64("core.timeout", 30), key.as_deref()) {
        Ok(_) => rep.ok(format!("ping: {} answered in {}ms", p.name, started.elapsed().as_millis())),
        Err(e) => rep.fail(e.code(), format!("ping: {e}")),
    }
}

#[cfg(unix)]
fn set_private(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::worse;

    #[test]
    fn exit_code_priority() {
        assert_eq!(worse(0, 1), 1);
        assert_eq!(worse(1, 3), 3);
        assert_eq!(worse(3, 4), 4);
        assert_eq!(worse(4, 3), 4);
        assert_eq!(worse(3, 1), 3);
    }
}
