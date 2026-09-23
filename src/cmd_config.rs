//! `hey config`: get, set, unset, list, edit, init, providers (spec section 3).

use crate::config::{
    self, Config, Scope, ScopeKind, is_secret_key, redact, validate, write_private,
};
use crate::error::{Error, Result, config_err, usage_err};
use crate::ini::KeyPath;
use crate::provider::Kind;
use crate::shell;
use crate::util::{display_path, shell_command};
use std::io::{BufRead, IsTerminal, Write};

const USAGE: &str = "\
usage: hey config get <key> [--local | --global]
       hey config set <key> [<value>] [--local | --global]
       hey config unset <key> [--local | --global]
       hey config list [--show-origin] [--show-secrets]
       hey config edit [--local | --global]
       hey config init
       hey config providers";

#[derive(Default)]
struct Flags {
    scope: Option<ScopeKind>,
    show_origin: bool,
    show_secrets: bool,
    positionals: Vec<String>,
}

fn parse_flags(args: Vec<String>) -> Result<Flags> {
    let mut f = Flags::default();
    for a in args {
        match a.as_str() {
            "--local" | "--global" => {
                let kind = if a == "--local" { ScopeKind::Local } else { ScopeKind::Global };
                if f.scope.is_some_and(|s| s != kind) {
                    return usage_err("--local and --global cannot be combined");
                }
                f.scope = Some(kind);
            }
            "--show-origin" => f.show_origin = true,
            "--show-secrets" => f.show_secrets = true,
            s if s.starts_with("--") => return usage_err(format!("unknown option {s}")),
            _ => f.positionals.push(a),
        }
    }
    Ok(f)
}

pub fn run(args: Vec<String>, shell_flag: Option<&str>) -> Result<()> {
    let mut it = args.into_iter();
    let Some(sub) = it.next() else {
        return usage_err(USAGE);
    };
    let flags = parse_flags(it.collect())?;
    match sub.as_str() {
        "get" => get(flags),
        "set" => set(flags),
        "unset" => unset(flags),
        "list" | "ls" => list(flags),
        "edit" => edit(flags),
        "init" => init(shell_flag),
        "providers" => providers(),
        other => usage_err(format!("unknown config command '{other}'\n{USAGE}")),
    }
}

/// Load the scope a write should go to (global unless `--local`).
fn writable_scope(kind: Option<ScopeKind>) -> Result<Scope> {
    match kind.unwrap_or(ScopeKind::Global) {
        ScopeKind::Global => {
            let path = config::global_path().ok_or_else(|| {
                Error::Config("cannot locate home directory; set HEY_CONFIG".into())
            })?;
            Scope::load(ScopeKind::Global, path).map_err(Error::Config)
        }
        ScopeKind::Local => Scope::load(ScopeKind::Local, config::local_path()).map_err(Error::Config),
    }
}

fn get(f: Flags) -> Result<()> {
    let [key] = f.positionals.as_slice() else {
        return usage_err("usage: hey config get <key> [--local | --global]");
    };
    let value = match f.scope {
        Some(kind) => {
            let scope = writable_scope(Some(kind))?;
            if scope.exists && is_secret_key(&KeyPath::parse(key).map(|k| k.dotted()).unwrap_or_default()) {
                check_scope_permissions(&scope)?;
            }
            scope.ini.get(key)
        }
        None => {
            let cfg = Config::load()?;
            if KeyPath::parse(key).is_some_and(|k| is_secret_key(&k.dotted())) {
                cfg.check_permissions()?;
            }
            cfg.get(key)
        }
    };
    match value {
        Some(v) => {
            outln!("{v}");
            Ok(())
        }
        None => config_err(format!("key not found: {key}")),
    }
}

fn check_scope_permissions(scope: &Scope) -> Result<()> {
    match config::insecure_mode(&scope.path) {
        Some(mode) => Err(Error::Refused(config::insecure_message(&scope.path, mode))),
        None => Ok(()),
    }
}

fn set(f: Flags) -> Result<()> {
    let mut pos = f.positionals.into_iter();
    let Some(key) = pos.next() else {
        return usage_err("usage: hey config set <key> [<value>] [--local | --global]");
    };
    let rest: Vec<String> = pos.collect();
    let Some(kp) = KeyPath::parse(&key) else {
        return usage_err(format!("invalid key '{key}' (expected section.key or section.name.key)"));
    };
    let dotted = kp.dotted();

    let value = if rest.is_empty() {
        // Only credentials may be read from a prompt, so they stay out of shell history.
        if !(is_secret_key(&dotted) && dotted.ends_with(".key")) {
            return usage_err(format!("missing value for {key}"));
        }
        let name = kp.sub.clone().unwrap_or_default();
        let entered = read_secret(&format!("API key for provider '{name}' (input hidden): "))?;
        if entered.trim().is_empty() {
            return usage_err("no key entered");
        }
        entered.trim().to_string()
    } else {
        rest.join(" ")
    };

    validate(&dotted, &value).map_err(Error::Usage)?;

    let mut scope = writable_scope(f.scope)?;
    scope.ini.set(&dotted, &value).map_err(Error::Usage)?;
    scope.save()?;
    if scope.kind == ScopeKind::Local && is_secret_key(&dotted) {
        eprintln!("hey: warning: stored a credential in ./.hey; keys belong in your global config (hey config set {key} --global)");
    }
    Ok(())
}

fn read_secret(prompt: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return usage_err("a hidden prompt needs a terminal; pass the value as an argument instead");
    }
    rpassword::prompt_password(prompt).map_err(|e| Error::Usage(format!("cannot read input: {e}")))
}

fn unset(f: Flags) -> Result<()> {
    let [key] = f.positionals.as_slice() else {
        return usage_err("usage: hey config unset <key> [--local | --global]");
    };
    let mut scope = writable_scope(f.scope)?;
    if !scope.ini.unset(key) {
        return config_err(format!("key not found in {} config: {key}", scope.kind.name()));
    }
    scope.save()
}

fn list(f: Flags) -> Result<()> {
    let cfg = Config::load()?;
    if f.show_secrets {
        cfg.check_permissions()?;
    }
    for scope in cfg.scopes() {
        if !scope.exists {
            continue;
        }
        let origin = display_path(&scope.path);
        for e in scope.ini.entries() {
            let shown = if e.key.ends_with(".key") && is_secret_key(&e.key) && !f.show_secrets {
                redact(&e.value)
            } else {
                e.value.replace('\n', "\\n")
            };
            if f.show_origin {
                outln!("{origin}\t{}={shown}", e.key);
            } else {
                outln!("{}={shown}", e.key);
            }
        }
    }
    Ok(())
}

fn edit(f: Flags) -> Result<()> {
    let scope = writable_scope(f.scope)?;
    if !scope.exists {
        write_private(&scope.path, "")
            .map_err(|e| Error::Config(format!("cannot create {}: {e}", display_path(&scope.path))))?;
    }
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_string());

    let status = if cfg!(windows) {
        let mut c = shell_command(&format!("{editor} \"{}\"", scope.path.display()));
        c.status()
    } else {
        // Run through sh so EDITOR="code -w" works; the path is a positional, not interpolated.
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$@\""))
            .arg("sh")
            .arg(&scope.path)
            .status()
    };
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(Error::Config(format!("editor exited with {s}"))),
        Err(e) => return Err(Error::Config(format!("cannot run editor '{editor}': {e}"))),
    }

    // Editors often rewrite the file with the default umask.
    let text = std::fs::read_to_string(&scope.path).unwrap_or_default();
    write_private(&scope.path, &text)
        .map_err(|e| Error::Config(format!("cannot secure {}: {e}", display_path(&scope.path))))?;
    if let Err(e) = crate::ini::Ini::parse(&text) {
        return config_err(format!("{}:{e}", display_path(&scope.path)));
    }
    Ok(())
}

fn providers() -> Result<()> {
    let cfg = Config::load()?;
    let active = std::env::var("HEY_PROVIDER")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| cfg.get("core.provider"))
        .unwrap_or_else(|| "anthropic".into());
    let escalate = cfg.get("core.escalate");

    let mut names = cfg.provider_names();
    // Built-ins that are in use but have no section of their own.
    for n in [Some(active.clone()), escalate.clone()].into_iter().flatten() {
        if Kind::parse(&n).is_some() && !names.contains(&n) {
            names.push(n);
        }
    }
    let rows: Vec<[String; 5]> = names
        .iter()
        .map(|n| {
            let mark = if *n == active {
                "*"
            } else if escalate.as_deref() == Some(n.as_str()) {
                "+"
            } else {
                " "
            };
            let kind = cfg
                .provider_field(n, "type")
                .or_else(|| Kind::parse(n).map(|k| k.name().to_string()))
                .unwrap_or_else(|| "?".into());
            let model = cfg
                .provider_field(n, "model")
                .or_else(|| Kind::parse(n).map(|k| k.default_model().to_string()))
                .unwrap_or_else(|| "-".into());
            let url = cfg
                .provider_field(n, "url")
                .or_else(|| Kind::parse(&kind).map(|k| k.default_url().to_string()))
                .unwrap_or_default();
            [mark.to_string(), n.clone(), kind, model, url]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let (w1, w2, w3) = (width(1), width(2), width(3));
    for r in &rows {
        outln!("{} {:<w1$}  {:<w2$}  {:<w3$}  {}", r[0], r[1], r[2], r[3], r[4]);
    }
    if !names.contains(&active) {
        eprintln!("hey: active provider '{active}' is not configured");
    }
    Ok(())
}

// ------------------------------------------------------------------ init ---

struct Preset {
    name: &'static str,
    kind: Kind,
    url: Option<&'static str>,
    model: Option<&'static str>,
    needs_key: bool,
}

const PRESETS: &[Preset] = &[
    Preset { name: "anthropic", kind: Kind::Anthropic, url: None, model: Some("claude-haiku-4-5"), needs_key: true },
    Preset { name: "openai", kind: Kind::OpenAi, url: None, model: Some("gpt-4.1-nano"), needs_key: true },
    Preset { name: "gemini", kind: Kind::Gemini, url: None, model: Some("gemini-2.5-flash"), needs_key: true },
    Preset { name: "grok", kind: Kind::OpenAi, url: Some("https://api.x.ai/v1"), model: None, needs_key: true },
    Preset { name: "ollama", kind: Kind::OpenAi, url: Some("http://localhost:11434/v1"), model: Some("llama3.1"), needs_key: false },
    Preset { name: "lmstudio", kind: Kind::OpenAi, url: Some("http://localhost:1234/v1"), model: None, needs_key: false },
    Preset { name: "custom", kind: Kind::OpenAi, url: None, model: None, needs_key: true },
];

fn ask_line(question: &str, default: Option<&str>) -> Result<String> {
    let mut out = std::io::stdout();
    match default {
        Some(d) => write!(out, "{question} [{d}]: "),
        None => write!(out, "{question}: "),
    }
    .and_then(|_| out.flush())
    .map_err(|e| Error::Usage(format!("cannot write prompt: {e}")))?;
    let mut line = String::new();
    let n = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Error::Usage(format!("cannot read input: {e}")))?;
    if n == 0 {
        return usage_err("input ended before setup finished");
    }
    let line = line.trim();
    Ok(if line.is_empty() { default.unwrap_or("").to_string() } else { line.to_string() })
}

fn ask_required(question: &str, default: Option<&str>) -> Result<String> {
    loop {
        let v = ask_line(question, default)?;
        if !v.is_empty() {
            return Ok(v);
        }
        outln!("A value is required.");
    }
}

fn yes(question: &str) -> Result<bool> {
    let a = ask_line(&format!("{question} (y/N)"), None)?;
    Ok(matches!(a.to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn init(shell_flag: Option<&str>) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return usage_err("hey config init is interactive and needs a terminal");
    }
    let mut scope = writable_scope(Some(ScopeKind::Global))?;
    outln!("hey setup. This writes {} (mode 0600).\n", display_path(&scope.path));

    outln!("Providers:");
    for (i, p) in PRESETS.iter().enumerate() {
        outln!("  {}) {}", i + 1, p.name);
    }
    let choice = ask_required("Provider (number or name)", Some("1"))?;
    let preset = choice
        .parse::<usize>()
        .ok()
        .and_then(|n| PRESETS.get(n.wrapping_sub(1)))
        .or_else(|| PRESETS.iter().find(|p| p.name == choice))
        .ok_or_else(|| Error::Usage(format!("unknown provider '{choice}'")))?;

    let (name, kind, url) = if preset.name == "custom" {
        let name = ask_required("Name for this provider", None)?;
        if KeyPath::parse(&format!("provider.{name}.type")).is_none() {
            return usage_err("provider names may only use letters, digits, '-' and '_'");
        }
        let url = ask_required("Base URL (OpenAI-compatible)", None)?;
        (name, Kind::OpenAi, Some(url))
    } else {
        (preset.name.to_string(), preset.kind, preset.url.map(String::from))
    };

    let key = if preset.needs_key {
        let k = read_secret("API key (input hidden, Enter to skip): ")?;
        let k = k.trim().to_string();
        if k.is_empty() {
            outln!("No key stored. Set HEY_{}_KEY or run: hey config set provider.{name}.key", name.to_ascii_uppercase().replace('-', "_"));
            None
        } else {
            Some(k)
        }
    } else {
        None
    };

    let model = ask_required("Model", preset.model)?;

    let set = |scope: &mut Scope, k: String, v: &str| scope.ini.set(&k, v).map_err(Error::Config);
    set(&mut scope, "core.provider".into(), &name)?;
    set(&mut scope, format!("provider.{name}.type"), kind.name())?;
    if let Some(u) = &url {
        set(&mut scope, format!("provider.{name}.url"), u)?;
    }
    set(&mut scope, format!("provider.{name}.model"), &model)?;
    if let Some(k) = &key {
        set(&mut scope, format!("provider.{name}.key"), k)?;
    }

    let big = ask_line("Escalate model for `hey -x` (blank to skip)", None)?;
    if !big.is_empty() {
        let big_name = format!("{name}-big");
        set(&mut scope, "core.escalate".into(), &big_name)?;
        set(&mut scope, format!("provider.{big_name}.type"), kind.name())?;
        if let Some(u) = &url {
            set(&mut scope, format!("provider.{big_name}.url"), u)?;
        }
        set(&mut scope, format!("provider.{big_name}.model"), &big)?;
        if key.is_some() {
            set(&mut scope, format!("provider.{big_name}.key_from"), &name)?;
        }
    }

    scope.save()?;
    outln!("\nWrote {}", display_path(&scope.path));

    // Offer shell integration only for a shell we can install into.
    let detected = crate::context::detect_shell(shell_flag).and_then(|s| shell::Sh::parse(&s).ok());
    if let Some(sh) = detected
        && yes(&format!("Install {} shell integration (adds the previous command and its exit status)?", sh.name()))?
    {
        let (profile, change) = shell::install(sh)?;
        let p = display_path(&profile);
        match change {
            shell::Change::Unchanged => outln!("Shell integration already current in {p}"),
            _ => outln!("Installed in {p}. Restart your shell or run: source {p}"),
        }
    }
    outln!("\nTry: hey how do I list files by size");
    Ok(())
}
