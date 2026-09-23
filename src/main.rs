//! hey: send terminal context plus an optional question to an LLM.

/// `println!` that ignores a closed pipe (`hey config list | head`) instead of panicking.
macro_rules! outln {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}

mod cmd_config;
mod config;
mod context;
mod doctor;
mod error;
mod ini;
mod output;
mod prompt;
mod provider;
mod shell;
mod util;

use config::{BUILTIN_SUBCOMMANDS, Config, DEFAULT_QUESTION};
use context::ShellInfo;
use error::{Error, Result, config_err, usage_err};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Instant;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
hey: ask an LLM about your terminal

Usage:
  hey [OPTIONS] [PROMPT...]
  <cmd> | hey [OPTIONS] [PROMPT...]
  hey <subcommand> [ARGS...]

Subcommands:
  config     get, set, unset, list, edit, init, providers
  shell      init, install, uninstall, status
  doctor     check configuration, permissions and shell integration
  version    print version information
  help       print this help (`hey help aliases` lists your aliases)

Options (before the prompt; use `--` to start a prompt with a dash):
  -p, --provider <name>   use this provider for this call
  -m, --model <name>      use this model for this call
  -x, --escalate          use the core.escalate provider (the big model)
  -n, --no-context        send only stdin and the prompt
      --dry-run           print the request (keys redacted) and exit
      --raw               print the raw response body
  -v, --verbose           log timing, provider, model, tokens to stderr
  -h, --help              print this help
  -V, --version           print version

Set by shell integration (not for manual use):
      --last-command <s>      the previous command line
      --current-command <s>   the line that invoked hey
      --exit-code <n>         exit status of the previous command
      --shell <name>          shell name

Pipe stderr too when you want it seen: cmd 2>&1 | hey why";

#[derive(Debug, Default)]
pub struct Opts {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub escalate: bool,
    pub no_context: bool,
    pub dry_run: bool,
    pub raw: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
    pub shell: ShellInfo,
}

enum Parsed {
    Subcommand { name: String, args: Vec<String>, opts: Opts },
    Ask { opts: Opts, words: Vec<String>, literal: bool },
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Silent(code)) => ExitCode::from(code),
        Err(e) => {
            eprintln!("hey: {e}");
            ExitCode::from(e.code())
        }
    }
}

fn run(args: Vec<String>) -> Result<()> {
    match parse_args(args)? {
        Parsed::Subcommand { name, args, opts } => run_subcommand(&name, args, opts),
        Parsed::Ask { opts, .. } if opts.help => {
            outln!("{USAGE}");
            Ok(())
        }
        Parsed::Ask { opts, .. } if opts.version => {
            print_version();
            Ok(())
        }
        Parsed::Ask { opts, words, literal } => ask(opts, words, literal),
    }
}

fn print_version() {
    outln!("hey {VERSION} (shell snippet v{})", shell::SNIPPET_VERSION);
}

fn run_subcommand(name: &str, args: Vec<String>, opts: Opts) -> Result<()> {
    match name {
        "config" => cmd_config::run(args, opts.shell.shell.as_deref()),
        "shell" => run_shell(args, opts.shell.shell.as_deref()),
        "doctor" => doctor::run(args),
        "version" => {
            print_version();
            Ok(())
        }
        "help" => run_help(args),
        _ => unreachable!("dispatch only passes builtin names"),
    }
}

fn run_help(args: Vec<String>) -> Result<()> {
    if args.first().map(String::as_str) == Some("aliases") {
        let cfg = Config::load()?;
        let names = cfg.alias_names();
        if names.is_empty() {
            outln!("No aliases. Create one: hey config set alias.<name>.prompt \"<text>\"");
        }
        for n in names {
            let prompt = cfg.alias_field(&n, "prompt").unwrap_or_default();
            outln!("{n}\t{prompt}");
        }
    } else {
        outln!("{USAGE}");
    }
    Ok(())
}

/// Argument resolution, section 1: options, then subcommand, alias, or prompt.
fn parse_args(args: Vec<String>) -> Result<Parsed> {
    let mut opts = Opts::default();
    let mut i = 0;
    let mut literal = false;

    // A value for an option, from `--opt=value` or the next argument.
    fn value(args: &[String], i: &mut usize, inline: Option<String>, flag: &str) -> Result<String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| Error::Usage(format!("option {flag} needs a value")))
    }

    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            literal = true;
            i += 1;
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match name {
                "provider" => opts.provider = Some(value(&args, &mut i, inline, "--provider")?),
                "model" => opts.model = Some(value(&args, &mut i, inline, "--model")?),
                "escalate" => opts.escalate = true,
                "no-context" => opts.no_context = true,
                "dry-run" => opts.dry_run = true,
                "raw" => opts.raw = true,
                "verbose" => opts.verbose = true,
                "help" => opts.help = true,
                "version" => opts.version = true,
                "last-command" => {
                    opts.shell.last_command = Some(value(&args, &mut i, inline, "--last-command")?)
                }
                "current-command" => {
                    opts.shell.current_command =
                        Some(value(&args, &mut i, inline, "--current-command")?)
                }
                "exit-code" => {
                    let v = value(&args, &mut i, inline, "--exit-code")?;
                    opts.shell.exit_code = Some(
                        v.trim()
                            .parse()
                            .map_err(|_| Error::Usage(format!("--exit-code needs an integer, got '{v}'")))?,
                    );
                }
                "shell" => opts.shell.shell = Some(value(&args, &mut i, inline, "--shell")?),
                _ => return usage_err(format!("unknown option --{name}. Try: hey help")),
            }
            i += 1;
        } else if a.len() > 1 && a.starts_with('-') {
            // Short options, possibly clustered: -vn, -pNAME, -p NAME.
            let cluster: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < cluster.len() {
                match cluster[k] {
                    'x' => opts.escalate = true,
                    'n' => opts.no_context = true,
                    'v' => opts.verbose = true,
                    'h' => opts.help = true,
                    'V' => opts.version = true,
                    c @ ('p' | 'm') => {
                        let rest: String = cluster[k + 1..].iter().collect();
                        let inline = (!rest.is_empty()).then_some(rest);
                        let flag = format!("-{c}");
                        let v = value(&args, &mut i, inline, &flag)?;
                        if c == 'p' {
                            opts.provider = Some(v);
                        } else {
                            opts.model = Some(v);
                        }
                        break;
                    }
                    c => return usage_err(format!("unknown option -{c}. Try: hey help")),
                }
                k += 1;
            }
            i += 1;
        } else {
            break; // first positional: everything from here is the prompt
        }
    }

    let rest: Vec<String> = args[i.min(args.len())..].to_vec();
    if !literal
        && let Some(first) = rest.first()
        && BUILTIN_SUBCOMMANDS.contains(&first.as_str())
    {
        return Ok(Parsed::Subcommand {
            name: first.clone(),
            args: rest[1..].to_vec(),
            opts,
        });
    }
    Ok(Parsed::Ask { opts, words: rest, literal })
}

fn run_shell(args: Vec<String>, flag_shell: Option<&str>) -> Result<()> {
    let mut shell_flag: Option<String> = flag_shell.map(String::from);
    let mut positionals = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--shell" {
            shell_flag = Some(it.next().ok_or_else(|| Error::Usage("--shell needs a value".into()))?);
        } else if let Some(v) = a.strip_prefix("--shell=") {
            shell_flag = Some(v.to_string());
        } else if a.starts_with('-') {
            return usage_err(format!("unknown option {a}"));
        } else {
            positionals.push(a);
        }
    }
    let sub = positionals.first().map(String::as_str).unwrap_or("");
    match sub {
        "init" => {
            let name = positionals.get(1).cloned().or(shell_flag);
            let sh = shell::Sh::resolve(name.as_deref())?;
            let _ = std::io::stdout().write_all(shell::block(sh, "\n").as_bytes());
            Ok(())
        }
        "install" => {
            let sh = shell::Sh::resolve(shell_flag.as_deref())?;
            let (profile, change) = shell::install(sh)?;
            let p = util::display_path(&profile);
            match change {
                shell::Change::Installed => outln!("Installed {} integration in {p}. Restart your shell or run: source {p}", sh.name()),
                shell::Change::Updated => outln!("Updated {} integration in {p}. Restart your shell or run: source {p}", sh.name()),
                shell::Change::Unchanged => outln!("{} integration in {p} is already current", sh.name()),
            }
            Ok(())
        }
        "uninstall" => {
            let sh = shell::Sh::resolve(shell_flag.as_deref())?;
            let (profile, removed) = shell::uninstall(sh)?;
            let p = util::display_path(&profile);
            if removed {
                outln!("Removed {} integration from {p}", sh.name());
            } else {
                outln!("{} integration was not installed in {p}", sh.name());
            }
            Ok(())
        }
        "status" => {
            let sh = shell::Sh::resolve(shell_flag.as_deref())?;
            let st = shell::status(sh)?;
            outln!("shell:   {}", sh.name());
            outln!("profile: {}", util::display_path(&st.profile));
            outln!("status:  {}", st.describe());
            Ok(())
        }
        "" => usage_err("usage: hey shell <init|install|uninstall|status> [--shell <bash|zsh|pwsh>]"),
        other => usage_err(format!("unknown shell command '{other}'")),
    }
}

/// The model call: build context, send, print.
fn ask(opts: Opts, words: Vec<String>, literal: bool) -> Result<()> {
    let started = Instant::now();
    if opts.escalate && opts.provider.is_some() {
        return usage_err("--escalate and --provider cannot be combined");
    }

    let cfg = Config::load()?;
    cfg.check_permissions()?;
    cfg.warn_local_keys();

    // Alias expansion (section 6).
    let mut alias: Option<String> = None;
    let mut words = words;
    if !literal
        && let Some(first) = words.first()
        && cfg.alias_field(first, "prompt").is_some()
    {
        alias = Some(words.remove(0));
    }
    let prompt_text = match &alias {
        Some(a) => {
            let base = cfg.alias_field(a, "prompt").unwrap_or_default();
            if words.is_empty() { base } else { format!("{base} {}", words.join(" ")) }
        }
        None => words.join(" "),
    };
    let alias_field = |f: &str| alias.as_deref().and_then(|a| cfg.alias_field(a, f));

    let env_nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let no_context = opts.no_context
        || env_nonempty("HEY_NO_CONTEXT").and_then(|v| util::is_truthy(&v)).unwrap_or(false)
        || alias_field("no_context").and_then(|v| util::is_truthy(&v)).unwrap_or(false);

    // Stdin, only when it is not a terminal.
    let max_stdin = cfg.get_u64("context.max_stdin_bytes", 32768) as usize;
    let stdin_text = if std::io::stdin().is_terminal() {
        None
    } else {
        context::read_capped(std::io::stdin().lock(), max_stdin)
            .map_err(|e| Error::Usage(format!("cannot read stdin: {e}")))?
    };

    if prompt_text.trim().is_empty() && stdin_text.is_none() {
        eprintln!("{USAGE}");
        return Err(Error::Silent(2));
    }
    let question = if prompt_text.trim().is_empty() {
        cfg.get("prompt.default_question").unwrap_or_else(|| DEFAULT_QUESTION.to_string())
    } else {
        prompt_text
    };

    // Provider and model selection.
    let flag_provider = opts.provider.is_some() || opts.escalate;
    let provider_name = if let Some(p) = &opts.provider {
        p.clone()
    } else if opts.escalate {
        match cfg.get("core.escalate") {
            Some(e) => e,
            None => {
                return config_err(
                    "core.escalate is not set. Run: hey config set core.escalate <provider>",
                );
            }
        }
    } else {
        env_nonempty("HEY_PROVIDER")
            .or_else(|| alias_field("provider"))
            .or_else(|| cfg.get("core.provider"))
            .unwrap_or_else(|| "anthropic".to_string())
    };
    let model_override = opts
        .model
        .clone()
        .or_else(|| if opts.escalate { None } else { env_nonempty("HEY_MODEL") })
        .or_else(|| if flag_provider { None } else { alias_field("model") });
    let prov = provider::resolve(&cfg, &provider_name, model_override.as_deref())?;

    // Context blocks. `producer | hey` can only be the form in use when stdin is
    // piped; otherwise the "current line" may be a stale history entry.
    let mut opts = opts;
    if stdin_text.is_none() {
        opts.shell.current_command = None;
    }
    let parts = assemble(&cfg, &opts, no_context, stdin_text, question);
    let user = prompt::user_message(&parts);
    let system = cfg.get("prompt.system").unwrap_or_default();
    let max_tokens = cfg.get_u64("core.max_tokens", 800);
    let timeout = cfg.get_u64("core.timeout", 30);
    let stream = cfg.get_bool("core.stream") && !opts.raw && !opts.dry_run;
    let params = provider::Params { system: &system, user: &user, max_tokens, stream };

    if opts.dry_run {
        // Never run key_cmd for a dry run; show that a credential would be sent.
        let placeholder = cfg.has_key_source(&prov.name).then_some("<redacted>");
        let req = provider::build_request(&prov, placeholder, &params);
        let _ = std::io::stdout().write_all(req.dry_run_text().as_bytes());
        return Ok(());
    }

    let key = cfg.resolve_key(&prov.name)?;
    if key.is_none() && prov.url == prov.kind.default_url() {
        return config_err(format!(
            "no API key for provider '{}'. Set {} or run: hey config set provider.{}.key",
            prov.name,
            config::env_key_name(&prov.name),
            prov.name
        ));
    }
    let req = provider::build_request(&prov, key.as_deref(), &params);

    if opts.verbose {
        eprintln!(
            "hey: provider={} type={} model={} url={}",
            prov.name, prov.kind.name(), prov.model, prov.url
        );
        eprintln!(
            "hey: prepared in {}ms, request body {} bytes{}",
            started.elapsed().as_millis(),
            req.body.to_string().len(),
            if stream { ", streaming" } else { "" }
        );
    }

    let sent_at = Instant::now();
    let usage;
    let stdout = std::io::stdout();
    let mode = output::Mode::detect(
        stdout.is_terminal(),
        cfg.get_bool("core.color"),
        std::env::var_os("NO_COLOR").is_some(),
    );

    if stream {
        let mut r = output::Renderer::new(stdout.lock(), mode);
        let mut wrote = false;
        let mut io_err: Option<std::io::Error> = None;
        usage = provider::send_stream(prov.kind, &req, timeout, key.as_deref(), |t| {
            if io_err.is_none() {
                wrote |= !t.is_empty();
                if let Err(e) = r.push(t) {
                    io_err = Some(e);
                }
            }
        })?;
        if let Some(e) = io_err {
            return io_result(Err(e));
        }
        io_result(r.finish())?;
        if !wrote {
            return Err(Error::Provider("the provider returned an empty response".into()));
        }
    } else {
        let sent = provider::send(prov.kind, &req, timeout, key.as_deref())?;
        usage = sent.usage.clone();
        if opts.raw {
            let mut out = stdout.lock();
            let nl = if sent.raw.ends_with('\n') { "" } else { "\n" };
            io_result(write!(out, "{}{nl}", sent.raw))?;
        } else {
            if sent.text.trim().is_empty() {
                return Err(Error::Provider("the provider returned an empty response".into()));
            }
            io_result(output::render_all(stdout.lock(), mode, &sent.text))?;
        }
    }

    if opts.verbose {
        let n = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        eprintln!(
            "hey: {}ms, tokens in={} out={}",
            sent_at.elapsed().as_millis(),
            n(usage.input),
            n(usage.output)
        );
    }
    Ok(())
}

/// A closed pipe on stdout is a normal way to end, not a failure.
fn io_result(r: std::io::Result<()>) -> Result<()> {
    match r {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(Error::Provider(format!("cannot write output: {e}")))
        }
        _ => Ok(()),
    }
}

fn assemble(
    cfg: &Config,
    opts: &Opts,
    no_context: bool,
    stdin_text: Option<String>,
    question: String,
) -> prompt::PromptParts {
    let mut parts = prompt::PromptParts { question, output: stdin_text, ..Default::default() };
    if no_context {
        return parts;
    }

    let want_system = cfg.get_bool("context.system");
    let shell_name = context::detect_shell(opts.shell.shell.as_deref())
        .filter(|_| want_system || opts.shell.shell.is_some());

    if want_system {
        let sys = context::system_info();
        for (k, v) in sys.lines {
            parts.environment.push((k.to_string(), v));
        }
    }
    if let Some(s) = shell_name {
        parts.environment.push(("shell".into(), s));
    }
    if want_system {
        if let Some(cwd) = context::cwd_string() {
            parts.environment.push(("cwd".into(), cwd));
        }
        if let Some(u) = context::username() {
            parts.environment.push(("user".into(), u));
        }
    }
    if cfg.get_bool("context.git")
        && let Some(g) = context::git_info()
    {
        parts.environment.push(("git".into(), g.summary()));
    }

    parts.command = context::command_of_interest(&opts.shell);
    // The command text can be lost (HISTCONTROL etc.); the status is still useful.
    if parts.command.is_none()
        && let Some(code) = opts.shell.exit_code
        && opts.shell.current_command.as_deref().and_then(context::pipe_producer).is_none()
    {
        parts.environment.push(("last_exit_code".into(), code.to_string()));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn ask_of(v: &[&str]) -> (Opts, Vec<String>, bool) {
        match parse_args(s(v)).unwrap() {
            Parsed::Ask { opts, words, literal } => (opts, words, literal),
            Parsed::Subcommand { name, .. } => panic!("unexpected subcommand {name}"),
        }
    }

    #[test]
    fn prompt_words_are_kept_in_order() {
        let (o, w, _) = ask_of(&["why", "did", "this", "fail"]);
        assert_eq!(w, ["why", "did", "this", "fail"]);
        assert!(!o.escalate);
    }

    #[test]
    fn options_only_count_before_the_first_word() {
        let (o, w, _) = ask_of(&["-x", "-m", "big", "how", "to", "tar", "-xzf", "-v"]);
        assert!(o.escalate && !o.verbose);
        assert_eq!(o.model.as_deref(), Some("big"));
        assert_eq!(w, ["how", "to", "tar", "-xzf", "-v"]);
    }

    #[test]
    fn long_short_and_clustered_forms() {
        let (o, _, _) = ask_of(&["--provider=openai", "-vn", "-mgpt", "q"]);
        assert_eq!(o.provider.as_deref(), Some("openai"));
        assert_eq!(o.model.as_deref(), Some("gpt"));
        assert!(o.verbose && o.no_context);
    }

    #[test]
    fn double_dash_forces_prompt_text() {
        let (_, w, literal) = ask_of(&["--", "config", "is", "what"]);
        assert!(literal);
        assert_eq!(w, ["config", "is", "what"]);
        let (_, w, _) = ask_of(&["-v", "--", "-x", "means"]);
        assert_eq!(w, ["-x", "means"]);
    }

    #[test]
    fn builtin_subcommands_take_the_remaining_args() {
        match parse_args(s(&["config", "set", "core.provider", "x", "--local"])).unwrap() {
            Parsed::Subcommand { name, args, .. } => {
                assert_eq!(name, "config");
                assert_eq!(args, ["set", "core.provider", "x", "--local"]);
            }
            _ => panic!(),
        }
        match parse_args(s(&["-v", "doctor", "--fix"])).unwrap() {
            Parsed::Subcommand { name, args, opts } => {
                assert_eq!((name.as_str(), args), ("doctor", s(&["--fix"])));
                assert!(opts.verbose);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn shell_integration_flags() {
        let (o, w, _) = ask_of(&[
            "--shell", "bash", "--exit-code", "128", "--last-command", "git push",
            "--current-command", "hey why", "why",
        ]);
        assert_eq!(o.shell.shell.as_deref(), Some("bash"));
        assert_eq!(o.shell.exit_code, Some(128));
        assert_eq!(o.shell.last_command.as_deref(), Some("git push"));
        assert_eq!(o.shell.current_command.as_deref(), Some("hey why"));
        assert_eq!(w, ["why"]);
    }

    #[test]
    fn empty_flag_values_are_accepted() {
        let (o, _, _) = ask_of(&["--last-command", "", "q"]);
        assert_eq!(o.shell.last_command.as_deref(), Some(""));
    }

    #[test]
    fn bad_usage_is_reported() {
        assert!(matches!(parse_args(s(&["--bogus"])), Err(Error::Usage(_))));
        assert!(matches!(parse_args(s(&["-z"])), Err(Error::Usage(_))));
        assert!(matches!(parse_args(s(&["-p"])), Err(Error::Usage(_))));
        assert!(matches!(parse_args(s(&["--exit-code", "abc"])), Err(Error::Usage(_))));
    }
}
