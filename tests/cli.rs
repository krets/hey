//! End-to-end tests: run the real binary against a mock HTTP server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;

// ----------------------------------------------------------------- harness

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "hey-it-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("work")).unwrap();
        Sandbox { root }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("home").join(".hey")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn home(&self) -> PathBuf {
        let h = self.root.join("home");
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    /// Write the global config with mode 0600.
    fn write_config(&self, text: &str) {
        self.home();
        let p = self.config_path();
        std::fs::write(&p, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_hey"));
        c.current_dir(self.work())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("HEY_CONFIG", self.config_path())
            .env("SHELL", "/bin/bash")
            .env("USER", "tester")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_with_stdin(mut c: Command, input: &str) -> Output {
    c.stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_string();
    let t = thread::spawn(move || {
        let _ = stdin.write_all(data.as_bytes());
    });
    let out = child.wait_with_output().unwrap();
    t.join().unwrap();
    out
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// Serves `responses` in order, one per connection, and reports each request.
fn mock(responses: Vec<(u16, &'static str, String)>) -> (String, Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = channel();
    thread::spawn(move || {
        for (status, ctype, body) in responses {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut headers = Vec::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap();
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            let _ = tx.send(Captured { path, headers, body: String::from_utf8(buf).unwrap() });
            let mut s = stream;
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.flush();
        }
    });
    (url, rx)
}

fn anthropic_ok(text: &str) -> (u16, &'static str, String) {
    (
        200,
        "application/json",
        serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 11, "output_tokens": 3}
        })
        .to_string(),
    )
}

fn openai_ok(text: &str) -> (u16, &'static str, String) {
    (
        200,
        "application/json",
        serde_json::json!({"choices": [{"message": {"content": text}}]}).to_string(),
    )
}

fn anthropic_config(url: &str) -> String {
    format!(
        "[core]\n    provider = anthropic\n\n[provider \"anthropic\"]\n    type = anthropic\n    url = {url}\n    model = test-model\n    key = sk-test-SECRET-1234\n"
    )
}

// ------------------------------------------------------------------- tests

#[test]
fn asks_anthropic_and_prints_only_the_answer() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("Use ls -la.")]);
    sb.write_config(&anthropic_config(&url));

    let out = sb.cmd().args(["how", "do", "I", "list", "files"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "Use ls -la.\n");
    assert_eq!(stderr(&out), "");

    let req = rx.recv().unwrap();
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.header("x-api-key"), Some("sk-test-SECRET-1234"));
    assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
    let body = req.json();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 800);
    let user = body["messages"][0]["content"].as_str().unwrap();
    assert!(user.contains("<environment>") && user.contains("shell: bash"), "{user}");
    assert!(user.ends_with("<question>\nhow do I list files\n</question>"), "{user}");
    assert!(body["system"].as_str().unwrap().starts_with("You are a command line assistant"));
}

#[test]
fn stdin_becomes_output_section_with_ansi_stripped_and_default_question() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));

    let out = run_with_stdin(sb.cmd(), "\x1b[31merror:\x1b[0m boom\n");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let user = rx.recv().unwrap().json()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(user.contains("<output>\nerror: boom\n</output>"), "{user}");
    assert!(user.contains("<question>\nExplain this output and, if it indicates an error, give the fix.\n</question>"));
}

#[test]
fn no_context_sends_only_stdin_and_prompt() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));
    let mut c = sb.cmd();
    c.args(["-n", "--last-command", "make", "--exit-code", "2", "why"]);
    let out = run_with_stdin(c, "log line\n");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let user = rx.recv().unwrap().json()["messages"][0]["content"].as_str().unwrap().to_string();
    assert_eq!(user, "<output>\nlog line\n</output>\n\n<question>\nwhy\n</question>");
}

#[test]
fn shell_flags_produce_command_block_bare_form() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb
        .cmd()
        .args([
            "--dry-run", "--shell", "zsh", "--exit-code", "128",
            "--last-command", "git push origin main", "--current-command", "hey why", "why",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("shell: zsh"), "{text}");
    assert!(text.contains("<command exit_code=\\\"128\\\">\\ngit push origin main\\n</command>"), "{text}");
}

#[test]
fn pipe_form_uses_producer_and_drops_exit_status() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let mut c = sb.cmd();
    c.args([
        "--dry-run", "--shell", "bash", "--exit-code", "0", "--last-command", "ls",
        "--current-command", "cargo build 2>&1 | hey why",
    ]);
    let out = run_with_stdin(c, "error[E0425]\n");
    let text = stdout(&out);
    assert!(text.contains("<command>\\ncargo build 2>&1\\n</command>"), "{text}");
    assert!(!text.contains("exit_code"), "{text}");
    assert!(!text.contains("ls\\n"), "{text}");
}

#[test]
fn long_stdin_is_truncated_head_and_tail() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&format!("{}\n[context]\n    max_stdin_bytes = 100\n", anthropic_config(&url)));
    let input = format!("START{}END", "x".repeat(2000));
    let out = run_with_stdin(sb.cmd(), &input);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let user = rx.recv().unwrap().json()["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(user.contains("START") && user.contains("END"), "{user}");
    assert!(user.contains("[... 1908 bytes truncated ...]"), "{user}");
}

#[test]
fn openai_type_with_custom_url_and_fence_stripping() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![openai_ok("```bash\ngit push -u origin main\n```")]);
    sb.write_config(&format!(
        "[core]\n provider = local\n[provider \"local\"]\n type = openai\n url = {url}/v1\n model = llama3.1\n"
    ));
    let out = sb.cmd().arg("fix").arg("this").output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    // stdout is a pipe here, so the fences are stripped.
    assert_eq!(stdout(&out), "git push -u origin main\n");
    let req = rx.recv().unwrap();
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(req.header("authorization"), None, "no key means no auth header");
    let body = req.json();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["max_tokens"], 800);
}

#[test]
fn gemini_type() {
    let sb = Sandbox::new();
    let body = serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi "},{"text":"there"}]}}]}).to_string();
    let (url, rx) = mock(vec![(200, "application/json", body)]);
    sb.write_config(&format!(
        "[core]\n provider = g\n[provider \"g\"]\n type = gemini\n url = {url}/v1beta\n model = gm\n key = gkey12345\n"
    ));
    let out = sb.cmd().arg("hello").output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "hi there\n");
    let req = rx.recv().unwrap();
    assert_eq!(req.path, "/v1beta/models/gm:generateContent");
    assert_eq!(req.header("x-goog-api-key"), Some("gkey12345"));
}

#[test]
fn streaming_prints_deltas() {
    let sb = Sandbox::new();
    let sse = [
        "event: message_start",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}",
        "",
        "event: content_block_delta",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}",
        "",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}",
        "",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}",
        "",
        "data: {\"type\":\"message_stop\"}",
        "",
    ]
    .join("\n");
    let (url, rx) = mock(vec![(200, "text/event-stream", sse)]);
    sb.write_config(&format!("{}\n[core]\n    stream = true\n", anthropic_config(&url)));
    let out = sb.cmd().args(["-v", "hi"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "Hello, world\n");
    assert_eq!(rx.recv().unwrap().json()["stream"], true);
    assert!(stderr(&out).contains("tokens in=9 out=5"), "{}", stderr(&out));
}

#[test]
fn raw_prints_the_response_body() {
    let sb = Sandbox::new();
    let (url, _rx) = mock(vec![anthropic_ok("hey")]);
    sb.write_config(&anthropic_config(&url));
    let out = sb.cmd().args(["--raw", "x"]).output().unwrap();
    assert_eq!(code(&out), 0);
    let v: serde_json::Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(v["content"][0]["text"], "hey");
}

#[test]
fn dry_run_redacts_keys_and_makes_no_request() {
    let sb = Sandbox::new();
    // Nothing is listening on this port; a request would fail.
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().args(["--dry-run", "why"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("POST http://127.0.0.1:9/v1/messages"), "{text}");
    assert!(text.contains("x-api-key: <redacted>"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

#[test]
fn verbose_logs_never_contain_the_key() {
    let sb = Sandbox::new();
    let (url, _rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));
    let out = sb.cmd().args(["-v", "x"]).output().unwrap();
    let err = stderr(&out);
    assert!(err.contains("provider=anthropic") && err.contains("model=test-model"), "{err}");
    assert!(err.contains("tokens in=11 out=3"), "{err}");
    assert!(!err.contains("SECRET"));
}

#[test]
fn provider_error_exits_1_without_leaking_the_key() {
    let sb = Sandbox::new();
    let body = r#"{"error":{"message":"invalid x-api-key: sk-test-SECRET-1234"}}"#.to_string();
    let (url, _rx) = mock(vec![(401, "application/json", body)]);
    sb.write_config(&anthropic_config(&url));
    let out = sb.cmd().arg("x").output().unwrap();
    assert_eq!(code(&out), 1);
    let err = stderr(&out);
    assert!(err.contains("HTTP 401") && err.contains("<redacted>"), "{err}");
    assert!(!err.contains("SECRET"), "{err}");
    assert_eq!(stdout(&out), "");
}

#[test]
fn unreachable_provider_exits_1() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().arg("x").output().unwrap();
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(stderr(&out).contains("cannot reach"));
}

#[test]
fn usage_error_without_prompt_or_stdin_exits_2_without_calling_api() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().output().unwrap();
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("Usage:"));
    assert_eq!(stdout(&out), "");
}

#[test]
fn unknown_option_exits_2() {
    let sb = Sandbox::new();
    let out = sb.cmd().arg("--nope").output().unwrap();
    assert_eq!(code(&out), 2);
}

#[test]
fn missing_key_exits_3_with_guidance() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = anthropic\n");
    let out = sb.cmd().arg("x").output().unwrap();
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains("HEY_ANTHROPIC_KEY"), "{}", stderr(&out));
}

#[test]
fn env_key_wins_over_file_key() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));
    let out = sb.cmd().env("HEY_ANTHROPIC_KEY", "from-env-key").arg("x").output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(rx.recv().unwrap().header("x-api-key"), Some("from-env-key"));
}

#[test]
fn zero_config_works_with_only_an_env_key() {
    // No config file at all: built-in provider, default model. The URL is the
    // real default, so only check the request we would send.
    let sb = Sandbox::new();
    let out = sb
        .cmd()
        .env("HEY_ANTHROPIC_KEY", "k-abcdefgh")
        .args(["--dry-run", "hello"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("POST https://api.anthropic.com/v1/messages"), "{text}");
    assert!(text.contains("\"model\": \"claude-haiku-4-5\""), "{text}");
}

#[test]
fn key_cmd_and_key_from() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&format!(
        "[core]\n provider = big\n[provider \"base\"]\n type = anthropic\n url = {url}\n model = m\n key_cmd = echo cmd-key-9999\n[provider \"big\"]\n type = anthropic\n url = {url}\n model = m2\n key_from = base\n"
    ));
    let out = sb.cmd().arg("x").output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(rx.recv().unwrap().header("x-api-key"), Some("cmd-key-9999"));
}

#[test]
fn escalate_flag_uses_the_escalate_provider() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&format!(
        "[core]\n provider = small\n escalate = big\n[provider \"small\"]\n type = anthropic\n url = {url}\n model = tiny\n key = k-small-1234\n[provider \"big\"]\n type = anthropic\n url = {url}\n model = huge\n key_from = small\n"
    ));
    let out = sb.cmd().args(["-x", "think"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(rx.recv().unwrap().json()["model"], "huge");
}

#[test]
fn escalate_without_config_exits_3() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().args(["-x", "think"]).output().unwrap();
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains("core.escalate"));
}

#[test]
fn provider_and_model_overrides_by_flag_and_env() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok"), anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));
    sb.cmd().args(["-m", "flag-model", "x"]).output().unwrap();
    assert_eq!(rx.recv().unwrap().json()["model"], "flag-model");
    sb.cmd().env("HEY_MODEL", "env-model").arg("x").output().unwrap();
    assert_eq!(rx.recv().unwrap().json()["model"], "env-model");
}

#[test]
fn aliases_expand_and_append_extra_words() {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        "{}\n[alias \"fix\"]\n    prompt = The previous command failed. Give the corrected command only.\n    no_context = true\n",
        anthropic_config("http://127.0.0.1:9")
    ));
    let out = sb.cmd().args(["--dry-run", "fix", "also", "keep", "--force"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("The previous command failed. Give the corrected command only. also keep --force"),
        "{text}"
    );
    assert!(!text.contains("<environment>"), "alias no_context must apply: {text}");

    // `--` bypasses alias expansion.
    let out = sb.cmd().args(["--dry-run", "--", "fix", "it"]).output().unwrap();
    let text = stdout(&out);
    assert!(text.contains("<question>\\nfix it\\n</question>"), "{text}");
}

#[test]
fn builtin_names_cannot_be_aliases() {
    let sb = Sandbox::new();
    sb.write_config("");
    let out = sb.cmd().args(["config", "set", "alias.doctor.prompt", "x"]).output().unwrap();
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("builtin"));
}

#[test]
fn custom_system_prompt_and_max_tokens() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&format!(
        "{}\n[core]\n    max_tokens = 42\n[prompt]\n    system = Be terse.\n",
        anthropic_config(&url)
    ));
    sb.cmd().arg("x").output().unwrap();
    let body = rx.recv().unwrap().json();
    assert_eq!(body["system"], "Be terse.");
    assert_eq!(body["max_tokens"], 42);
}

#[test]
fn git_context_is_off_by_default_and_opt_in() {
    let sb = Sandbox::new();
    sb.write_config(&format!("{}\n[context]\n    git = true\n", anthropic_config("http://127.0.0.1:9")));
    let git = |args: &[&str]| {
        Command::new("git").args(args).current_dir(sb.work()).output().map(|o| o.status.success())
    };
    if git(&["init", "-q", "-b", "trunk"]).unwrap_or(false) {
        std::fs::write(sb.work().join("new.txt"), "x").unwrap();
        let out = sb.cmd().args(["--dry-run", "x"]).output().unwrap();
        let text = stdout(&out);
        assert!(text.contains("git: branch trunk"), "{text}");
        assert!(text.contains("1 untracked"), "{text}");
    }
    let sb2 = Sandbox::new();
    sb2.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb2.cmd().args(["--dry-run", "x"]).output().unwrap();
    assert!(!stdout(&out).contains("git:"));
}

// ------------------------------------------------------------ permissions

#[cfg(unix)]
mod unix_only {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn insecure_permissions_are_refused_with_exit_4() {
        let sb = Sandbox::new();
        sb.write_config(&anthropic_config("http://127.0.0.1:9"));
        std::fs::set_permissions(sb.config_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = sb.cmd().arg("x").output().unwrap();
        assert_eq!(code(&out), 4);
        let err = stderr(&out);
        assert!(err.contains("is readable by others (mode 0644)"), "{err}");
        assert!(err.contains("chmod 600") && err.contains("hey doctor --fix"), "{err}");
        assert_eq!(stdout(&out), "");
    }

    #[test]
    fn doctor_reports_and_fixes_permissions() {
        let sb = Sandbox::new();
        sb.write_config(&anthropic_config("http://127.0.0.1:9"));
        std::fs::set_permissions(sb.config_path(), std::fs::Permissions::from_mode(0o664)).unwrap();

        let out = sb.cmd().arg("doctor").output().unwrap();
        assert_eq!(code(&out), 4, "{}", stdout(&out));
        assert!(stdout(&out).contains("FAIL  permissions"), "{}", stdout(&out));

        let out = sb.cmd().args(["doctor", "--fix"]).output().unwrap();
        assert_eq!(code(&out), 0, "{}", stdout(&out));
        assert!(stdout(&out).contains("now 0600"), "{}", stdout(&out));
        assert_eq!(mode(&sb.config_path()), 0o600);
    }

    #[test]
    fn config_writes_create_a_0600_file() {
        let sb = Sandbox::new();
        let out = sb.cmd().args(["config", "set", "core.provider", "openai"]).output().unwrap();
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        assert_eq!(mode(&sb.config_path()), 0o600);
    }

    #[test]
    fn writes_repair_loose_permissions() {
        let sb = Sandbox::new();
        sb.write_config("[core]\n provider = a\n");
        std::fs::set_permissions(sb.config_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        sb.cmd().args(["config", "set", "core.timeout", "10"]).output().unwrap();
        assert_eq!(mode(&sb.config_path()), 0o600);
    }

    #[test]
    fn local_keys_in_an_unignored_git_tree_are_not_used() {
        let sb = Sandbox::new();
        let work = sb.work();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(&work).output();
        if !ok.map(|o| o.status.success()).unwrap_or(false) {
            return; // git unavailable
        }
        sb.write_config("[core]\n provider = anthropic\n");
        std::fs::write(
            work.join(".hey"),
            "[provider \"anthropic\"]\n    key = sk-local-LEAKED-key\n",
        )
        .unwrap();
        std::fs::set_permissions(work.join(".hey"), std::fs::Permissions::from_mode(0o600)).unwrap();

        let out = sb.cmd().args(["--dry-run", "x"]).output().unwrap();
        let text = stdout(&out);
        assert!(stderr(&out).contains("ignoring those keys"), "{}", stderr(&out));
        // No key source is left, so nothing would be sent as credentials.
        assert!(!text.contains("x-api-key"), "{text}");

        // Once the file is git-ignored the keys are usable again (with a warning).
        std::fs::write(work.join(".gitignore"), ".hey\n").unwrap();
        let out = sb.cmd().args(["--dry-run", "x"]).output().unwrap();
        assert!(stdout(&out).contains("x-api-key: <redacted>"), "{}", stdout(&out));
        assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    }

    #[test]
    fn local_config_overrides_per_key_without_key_warnings() {
        let sb = Sandbox::new();
        sb.write_config(&anthropic_config("http://127.0.0.1:9"));
        std::fs::write(sb.work().join(".hey"), "[provider \"anthropic\"]\n    model = local-model\n").unwrap();
        // A world-readable local file without keys is fine.
        std::fs::set_permissions(sb.work().join(".hey"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = sb.cmd().args(["--dry-run", "x"]).output().unwrap();
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        assert!(stdout(&out).contains("local-model"));
        assert_eq!(stderr(&out), "");
    }
}

// ------------------------------------------------------------ hey config

#[test]
fn config_set_get_unset_list_roundtrip_preserving_comments() {
    let sb = Sandbox::new();
    sb.write_config("# my notes\n[core]\n    provider = anthropic   # main\n\n[provider \"anthropic\"]\n    key = sk-ant-api03-abcdefWXYZ\n");

    let run = |args: &[&str]| sb.cmd().arg("config").args(args).output().unwrap();

    assert_eq!(stdout(&run(&["get", "core.provider"])), "anthropic\n");
    assert_eq!(code(&run(&["set", "core.provider", "openai"])), 0);
    assert_eq!(code(&run(&["set", "alias.fix.prompt", "Give the fix; be brief # ok"])), 0);
    assert_eq!(stdout(&run(&["get", "alias.fix.prompt"])), "Give the fix; be brief # ok\n");

    let file = std::fs::read_to_string(sb.config_path()).unwrap();
    assert!(file.starts_with("# my notes\n[core]\n    provider = openai   # main\n"), "{file}");

    // Redacted by default, visible on request.
    let list = stdout(&run(&["list"]));
    assert!(list.contains("provider.anthropic.key=sk-a...WXYZ"), "{list}");
    assert!(!list.contains("abcdef"), "{list}");
    assert!(stdout(&run(&["list", "--show-secrets"])).contains("sk-ant-api03-abcdefWXYZ"));
    let origin = stdout(&run(&["list", "--show-origin"]));
    assert!(origin.lines().all(|l| l.contains('\t')), "{origin}");
    assert!(origin.contains(".hey\tcore.provider=openai"), "{origin}");

    assert_eq!(code(&run(&["unset", "alias.fix.prompt"])), 0);
    assert_eq!(code(&run(&["get", "alias.fix.prompt"])), 3);
    assert_eq!(code(&run(&["unset", "alias.fix.prompt"])), 3);
    let file = std::fs::read_to_string(sb.config_path()).unwrap();
    assert!(!file.contains("alias"), "{file}");
}

#[test]
fn config_local_scope_writes_to_dot_hey_in_cwd() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = a\n");
    let out = sb.cmd().args(["config", "set", "core.provider", "b", "--local"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(sb.work().join(".hey").exists());
    assert_eq!(stdout(&sb.cmd().args(["config", "get", "core.provider"]).output().unwrap()), "b\n");
    assert_eq!(stdout(&sb.cmd().args(["config", "get", "core.provider", "--global"]).output().unwrap()), "a\n");
}

#[test]
fn config_set_validates_typed_keys() {
    let sb = Sandbox::new();
    let out = sb.cmd().args(["config", "set", "core.max_tokens", "lots"]).output().unwrap();
    assert_eq!(code(&out), 2);
    let out = sb.cmd().args(["config", "set", "provider.x.type", "cohere"]).output().unwrap();
    assert_eq!(code(&out), 2);
}

#[test]
fn config_set_key_without_value_needs_a_terminal() {
    let sb = Sandbox::new();
    let out = sb.cmd().args(["config", "set", "provider.x.key"]).output().unwrap();
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("terminal"), "{}", stderr(&out));
    assert!(!sb.config_path().exists());
    let out = sb.cmd().args(["config", "set", "core.provider"]).output().unwrap();
    assert!(stderr(&out).contains("missing value"));
}

#[test]
fn config_parse_errors_exit_3_with_location() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = a\n[broken\n");
    let out = sb.cmd().args(["config", "list"]).output().unwrap();
    assert_eq!(code(&out), 3);
    assert!(stderr(&out).contains(".hey:line 3"), "{}", stderr(&out));
}

#[test]
fn config_providers_marks_active_and_escalate() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = a\n escalate = b\n[provider \"a\"]\n type = anthropic\n model = m1\n[provider \"b\"]\n type = openai\n model = m2\n url = http://x/v1\n");
    let out = sb.cmd().args(["config", "providers"]).output().unwrap();
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("* a") && lines[0].contains("m1"), "{text}");
    assert!(lines[1].starts_with("+ b") && lines[1].contains("http://x/v1"), "{text}");
}

#[test]
fn config_edit_runs_the_editor_and_rechecks_the_file() {
    let sb = Sandbox::new();
    let out = sb
        .cmd()
        .env("VISUAL", "true")
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(sb.config_path().exists());

    // An editor that writes garbage is reported as a config error.
    let script = sb.root.join("bad-editor.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'not ini\\n' > \"$1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = sb
            .cmd()
            .env("EDITOR", &script)
            .args(["config", "edit"])
            .output()
            .unwrap();
        assert_eq!(code(&out), 3, "{}", stderr(&out));
    }
}

#[test]
fn config_init_needs_a_terminal() {
    let sb = Sandbox::new();
    let out = sb.cmd().args(["config", "init"]).output().unwrap();
    assert_eq!(code(&out), 2);
}

// ------------------------------------------------------- shell integration

#[test]
fn shell_init_prints_a_marked_snippet() {
    let sb = Sandbox::new();
    let out = sb.cmd().args(["shell", "init", "bash"]).output().unwrap();
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    assert!(text.starts_with("# >>> hey shell integration v1 >>>\n"));
    assert!(text.trim_end().ends_with("# <<< hey shell integration <<<"));
    assert!(text.contains("command hey --shell bash"));
    assert!(stdout(&sb.cmd().args(["shell", "init", "zsh"]).output().unwrap()).contains("--shell zsh"));
    assert!(stdout(&sb.cmd().args(["shell", "init", "pwsh"]).output().unwrap()).contains("Get-History"));
    assert_eq!(code(&sb.cmd().args(["shell", "init", "fish"]).output().unwrap()), 2);
}

#[test]
fn shell_install_status_uninstall_lifecycle() {
    let sb = Sandbox::new();
    let rc = sb.home().join(".bashrc");
    std::fs::write(&rc, "export A=1\n").unwrap();
    let shell = |args: &[&str]| sb.cmd().arg("shell").args(args).output().unwrap();

    assert!(stdout(&shell(&["status"])).contains("not installed"));

    let out = shell(&["install"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let after = std::fs::read_to_string(&rc).unwrap();
    assert!(after.starts_with("export A=1\n\n# >>> hey shell integration v1 >>>"), "{after}");
    let bak = sb.home().join(".bashrc.hey.bak");
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), "export A=1\n");
    assert!(stdout(&shell(&["status"])).contains("(current)"));

    // Idempotent: no duplicate, no change.
    let out = shell(&["install"]);
    assert!(stdout(&out).contains("already current"), "{}", stdout(&out));
    assert_eq!(std::fs::read_to_string(&rc).unwrap(), after);

    // A stale block is replaced in place and reported.
    let stale = after.replace("integration v1 >>>", "integration v0 >>>");
    std::fs::write(&rc, &stale).unwrap();
    assert!(stdout(&shell(&["status"])).contains("out of date"));
    shell(&["install"]);
    assert_eq!(std::fs::read_to_string(&rc).unwrap(), after);
    // The backup still holds the original, first-touch content.
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), "export A=1\n");

    shell(&["uninstall"]);
    assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export A=1\n");
    assert!(stdout(&shell(&["uninstall"])).contains("was not installed"));
}

#[test]
fn shell_flag_beats_the_shell_env_var() {
    let sb = Sandbox::new();
    let out = sb.cmd().args(["shell", "install", "--shell", "zsh"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(sb.home().join(".zshrc").exists());
    assert!(!sb.home().join(".bashrc").exists());
}

#[test]
fn shell_creates_a_missing_profile_without_a_backup() {
    let sb = Sandbox::new();
    sb.cmd().args(["shell", "install"]).output().unwrap();
    assert!(sb.home().join(".bashrc").exists());
    assert!(!sb.home().join(".bashrc.hey.bak").exists());
}

// --------------------------------------------------------- misc commands

#[test]
fn version_and_help() {
    let sb = Sandbox::new();
    let out = sb.cmd().arg("version").output().unwrap();
    assert_eq!(stdout(&out), format!("hey {} (shell snippet v1)\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(stdout(&sb.cmd().arg("--version").output().unwrap()), stdout(&out));
    let help = stdout(&sb.cmd().arg("help").output().unwrap());
    assert!(help.contains("Usage:") && help.contains("--dry-run"));
}

#[test]
fn help_aliases_lists_prompts() {
    let sb = Sandbox::new();
    sb.write_config("[alias \"explain\"]\n prompt = Explain in two sentences.\n");
    let out = sb.cmd().args(["help", "aliases"]).output().unwrap();
    assert_eq!(stdout(&out), "explain\tExplain in two sentences.\n");
}

#[test]
fn doctor_all_green_with_a_good_setup_and_optional_shell() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().arg("doctor").output().unwrap();
    let text = stdout(&out);
    assert_eq!(code(&out), 0, "{text}");
    assert!(text.contains("ok    config: global"), "{text}");
    assert!(text.contains("ok    provider: anthropic (type anthropic, model test-model"), "{text}");
    assert!(text.contains("ok    key: resolvable"), "{text}");
    assert!(text.contains("info  shell: bash integration not installed"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

#[test]
fn doctor_flags_missing_keys_and_bad_syntax() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = anthropic\n");
    let out = sb.cmd().arg("doctor").output().unwrap();
    assert_eq!(code(&out), 3);
    assert!(stdout(&out).contains("FAIL  key: none for 'anthropic'"), "{}", stdout(&out));

    sb.write_config("[core\n");
    let out = sb.cmd().arg("doctor").output().unwrap();
    assert_eq!(code(&out), 3);
    assert!(stdout(&out).contains("FAIL  config: global"), "{}", stdout(&out));
}

#[test]
fn doctor_ping_measures_latency() {
    let sb = Sandbox::new();
    let (url, rx) = mock(vec![anthropic_ok("ok")]);
    sb.write_config(&anthropic_config(&url));
    let out = sb.cmd().args(["doctor", "--ping"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("ok    ping: anthropic answered in"), "{}", stdout(&out));
    assert_eq!(rx.recv().unwrap().json()["max_tokens"], 16);
}

#[test]
fn doctor_ping_failure_exits_1() {
    let sb = Sandbox::new();
    sb.write_config(&anthropic_config("http://127.0.0.1:9"));
    let out = sb.cmd().args(["doctor", "--ping"]).output().unwrap();
    assert_eq!(code(&out), 1, "{}", stdout(&out));
}

#[test]
fn a_closed_stdout_pipe_is_not_a_crash() {
    let sb = Sandbox::new();
    sb.write_config("[core]\n provider = a\n");
    let mut child = sb
        .cmd()
        .args(["config", "list"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // reader goes away immediately
    let status = child.wait().unwrap();
    assert!(status.code().is_some(), "terminated by signal");
}
