//! End-to-end CLI tests: the commands an operator actually types.
//!
//! Each test gets its own config/env pair via `UMSP_CONFIG` / `UMSP_ENV_FILE`,
//! so nothing here touches the real ~/.config/umsp.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: PathBuf,
    config: PathBuf,
    env_file: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("umsp-cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, include_str!("../config.example.toml")).unwrap();
        let env_file = dir.join("env");
        std::fs::write(&env_file, "").unwrap();
        Self {
            dir,
            config,
            env_file,
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_umsp"))
            .args(args)
            .env("UMSP_CONFIG", &self.config)
            .env("UMSP_ENV_FILE", &self.env_file)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn umsp");
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run(args, stdin);
        assert!(
            out.status.success(),
            "{args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn read_env(&self) -> String {
        std::fs::read_to_string(&self.env_file).unwrap_or_default()
    }

    fn read_config(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn env_set_never_prints_the_secret() {
    let sb = Sandbox::new("env");
    let stdout = sb.ok(
        &["env", "set", "FOO_KEY", "--stdin"],
        Some("sk-secret-1234567890\n"),
    );
    assert!(
        stdout.contains("sk-se…90"),
        "masked value expected: {stdout}"
    );
    assert!(!stdout.contains("1234567890"), "secret leaked: {stdout}");
    assert!(sb.read_env().contains("FOO_KEY=sk-secret-1234567890"));

    let listed = sb.ok(&["env", "list", "--json"], None);
    assert!(listed.contains("\"name\":\"FOO_KEY\""));
    assert!(!listed.contains("sk-secret-1234567890"));

    sb.ok(&["env", "unset", "FOO_KEY"], None);
    assert!(!sb.read_env().contains("FOO_KEY"));
}

#[test]
fn id_hint_is_slugified() {
    let sb = Sandbox::new("slug");
    let out = sb.ok(
        &[
            "provider",
            "add",
            "--vendor",
            "deepseek",
            "--id",
            "My Provider",
            "--no-key",
        ],
        None,
    );
    assert!(out.contains("added provider my-provider"), "{out}");
    assert!(!out.contains("was taken"), "nothing was taken: {out}");
}

#[test]
fn identical_providers_get_distinct_ids() {
    let sb = Sandbox::new("independent");
    for _ in 0..2 {
        sb.ok(
            &["provider", "add", "--vendor", "deepseek", "--no-key"],
            None,
        );
    }
    let listed = sb.ok(&["provider", "list", "--json"], None);
    assert!(listed.contains("\"id\":\"deepseek\""), "{listed}");
    assert!(listed.contains("\"id\":\"deepseek-2\""), "{listed}");
    assert!(
        listed.contains("\"vendor\":\"deepseek\""),
        "instances keep their vendor: {listed}"
    );
    // The template has one deepseek entry; two more share the same endpoint.
    assert_eq!(
        sb.read_config()
            .matches("base_url = \"https://api.deepseek.com\"")
            .count(),
        3,
        "both entries must stay in the file, independently"
    );
}

#[test]
fn vendor_list_shows_the_catalogue() {
    let sb = Sandbox::new("vendors");
    let listed = sb.ok(&["vendor", "list", "--json"], None);
    for vendor in ["open-design", "deepseek"] {
        assert!(listed.contains(&format!("\"id\":\"{vendor}\"")), "{listed}");
    }
    assert!(
        listed.contains("\"configured\":1"),
        "the template has one instance of each vendor: {listed}"
    );
    assert!(listed.contains("https://api.deepseek.com"));
}

#[test]
fn grok_accounts_are_independent_of_each_other_and_of_api_keys() {
    let sb = Sandbox::new("grok-oauth");
    let first = sb.ok(&["provider", "add", "--vendor", "grok"], None);
    assert!(first.contains("umsp login grok"), "{first}");
    assert!(!first.contains("grok-2"), "{first}");
    let second = sb.ok(&["provider", "add", "--vendor", "grok"], None);
    assert!(second.contains("grok-2"), "{second}");
    assert!(second.contains("umsp login grok-2"), "{second}");
    let cfg = sb.read_config();
    let live_oauth = cfg
        .lines()
        .filter(|line| line.trim() == "auth = \"oauth\"")
        .count();
    assert_eq!(live_oauth, 2, "{cfg}");
    assert!(
        !cfg.contains("GROK_API_KEY") && !cfg.contains("GROK_2_API_KEY"),
        "oauth providers must not invent an env key: {cfg}"
    );
    assert!(sb.read_env().trim().is_empty(), "env file must stay empty");

    let check = sb.run(&["check", "--json"], None);
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(text.contains("umsp login grok`"), "{text}");
    assert!(text.contains("umsp login grok-2"), "{text}");

    let sessions = sb.dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("grok.json"),
        r#"{"access_token":"a","refresh_token":"r","expires_at":"2099-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let logged_out = sb.ok(&["logout", "grok"], None);
    assert!(
        logged_out.contains("signed out provider grok"),
        "{logged_out}"
    );
    assert!(!sessions.join("grok.json").exists());
    let missing = sb.run(&["logout", "grok-2"], None);
    assert!(!missing.status.success());
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(err.contains("grok-2"), "{err}");
}

#[test]
fn provider_add_needs_a_known_vendor() {
    let sb = Sandbox::new("unknown-vendor");
    let out = sb.run(
        &["provider", "add", "--vendor", "openrouter", "--no-key"],
        None,
    );
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("unknown vendor 'openrouter'"), "{text}");
    assert!(
        text.contains("open-design, deepseek, grok, chatgpt, qwen-token-plan, opencode-go"),
        "{text}"
    );
    assert!(!sb.read_config().contains("openrouter"));
}

#[test]
fn provider_add_qwen_token_plan_writes_the_plan_endpoints() {
    let sb = Sandbox::new("qwen-token-plan");
    sb.ok(
        &["provider", "add", "--vendor", "qwen-token-plan", "--no-key"],
        None,
    );
    let cfg = sb.read_config();
    assert!(cfg.contains("vendor = \"qwen-token-plan\""), "{cfg}");
    assert!(
        cfg.contains("https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1"),
        "{cfg}"
    );
    assert!(
        cfg.contains("https://token-plan.maas.qianwenaiapi.com/apps/anthropic"),
        "{cfg}"
    );
    assert!(cfg.contains("QWEN_TOKEN_PLAN_API_KEY"), "{cfg}");
    assert!(
        cfg.contains("protocols = [\"chat\", \"responses\", \"messages\"]"),
        "{cfg}"
    );
}

#[test]
fn provider_add_reads_the_key_and_backends_can_be_wired() {
    let sb = Sandbox::new("provider");
    let out = sb.ok(
        &[
            "provider",
            "add",
            "--vendor",
            "deepseek",
            "--id",
            "deepseek-personal",
            "--api-key-env",
            "DEEPSEEK_PERSONAL_API_KEY",
            "--json",
        ],
        Some("sk-personal-abcdef123456\n"),
    );
    assert!(out.contains("\"key_stored\":true"), "{out}");
    assert!(
        sb.read_env()
            .contains("DEEPSEEK_PERSONAL_API_KEY=sk-personal")
    );
    assert!(sb.read_config().contains("id = \"deepseek-personal\""));

    // Prepend a backend: it must become the first (highest priority) entry.
    sb.ok(
        &[
            "model",
            "backend",
            "add",
            "claude-haiku-5.5",
            "--backend",
            "deepseek-personal:deepseek-flash",
            "--first",
            "--json",
        ],
        None,
    );
    let listed = sb.ok(&["model", "list", "--json"], None);
    let first = listed
        .find("claude-haiku-5.5")
        .expect("model in list output");
    let tail = &listed[first..];
    let personal = tail.find("deepseek-personal").expect("backend in list");
    let open_design = tail.find("open-design").expect("backend in list");
    assert!(
        personal < open_design,
        "--first must put the new backend in front: {tail}"
    );

    // The last backend cannot be removed.
    let out = sb.run(
        &[
            "model",
            "backend",
            "remove",
            "claude-haiku-5.5",
            "--provider",
            "deepseek-personal",
        ],
        None,
    );
    assert!(out.status.success());
    let out = sb.run(
        &[
            "model",
            "backend",
            "remove",
            "claude-haiku-5.5",
            "--provider",
            "open-design",
        ],
        None,
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("last backend")
            || String::from_utf8_lossy(&out.stdout).contains("last backend")
    );
}
