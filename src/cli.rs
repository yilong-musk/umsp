//! Command-line surface. Every leaf command exists in `-h`; `logs` and
//! `completion` are functional but intentionally small in M0.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Debug, Parser)]
#[command(
    name = "umsp",
    version,
    about = "umsp — lightweight local AI gateway: one binary, three protocols",
    long_about = "umsp is a single-binary, loopback-only AI gateway.\n\
                  It exposes OpenAI Chat Completions / Responses and Anthropic Messages\n\
                  on one port and forwards each protocol to the upstream that speaks it.\n\n\
                  Data plane: `umsp serve`. Everything else is control plane.",
    propagate_version = true
)]
pub struct Cli {
    /// Config file (default: $UMSP_CONFIG or ~/.config/umsp/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Emit one line of JSON on stdout
    #[arg(long, global = true)]
    pub json: bool,

    /// Do not SIGHUP a running server after config edits
    #[arg(long, global = true)]
    pub no_reload: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP data plane (blocks; prints a ready line on stdout)
    Serve(ServeArgs),
    /// Create config.toml, the env file and the state directory
    Init(InitArgs),
    /// Inspect the configuration file
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage upstream API keys in the env file (values are never printed)
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Validate config + env (exit 10 when invalid)
    Check(CheckArgs),
    /// Deeper environment diagnostics
    Doctor(DoctorArgs),
    /// Manage upstream providers
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
    /// List the built-in vendors a provider can be created from
    Vendor {
        #[command(subcommand)]
        cmd: VendorCmd,
    },
    /// Manage the local model registry
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Read upstream model lists into the running gateway
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCmd,
    },
    /// Manage gateway clients (keys + model allow-lists)
    Client {
        #[command(subcommand)]
        cmd: ClientCmd,
    },
    /// Alias of `client` (list/add/revoke)
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Query the request ledger
    Usage {
        #[command(subcommand)]
        cmd: UsageCmd,
    },
    /// One minimal upstream request against a provider
    Probe(ProbeArgs),
    /// One Chat Completions call through the configured upstream
    Call(CallArgs),
    /// Sign one Grok subscription provider in (device code). Each provider id is its own account.
    Login(LoginArgs),
    /// Delete one provider's stored subscription session
    Logout(LogoutArgs),
    /// Read the JSONL request ledger
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },
    /// Print version and build target
    Version,
    /// Generate a shell completion script
    Completion {
        /// Shell to generate for
        shell: Shell,
    },
}

#[derive(Debug, clap::Args)]
pub struct LoginArgs {
    /// Provider id of an oauth instance (`umsp provider list`)
    pub provider: String,
    /// Do not try to open a browser; print the URL and code only
    #[arg(long)]
    pub no_browser: bool,
}

#[derive(Debug, clap::Args)]
pub struct LogoutArgs {
    /// Provider id whose session file should be removed
    pub provider: String,
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Override server.listen for this run
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// State directory (default: ~/.local/share/umsp)
    #[arg(long, value_name = "PATH")]
    pub state_dir: Option<PathBuf>,
    /// Overwrite an existing config.toml
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the resolved config path
    Path,
    /// Print the config file contents
    Show {
        /// Hide client key hashes and prefixes
        #[arg(long)]
        redact: bool,
    },
    /// Parse + validate the config file
    Validate,
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {}

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Also probe each provider over the network
    #[arg(long)]
    pub online: bool,
}

#[derive(Debug, Subcommand)]
pub enum EnvCmd {
    /// List variables with masked values
    List,
    /// Set one variable (hidden prompt on a TTY, otherwise reads stdin)
    Set {
        name: String,
        /// Read the value from stdin instead of prompting
        #[arg(long)]
        stdin: bool,
    },
    /// Remove one variable
    Unset { name: String },
}

#[derive(Debug, Subcommand)]
pub enum CatalogCmd {
    /// Re-read every provider model list now
    Refresh,
}

#[derive(Debug, Subcommand)]
pub enum VendorCmd {
    /// List supported vendors (ids, endpoints, protocols, quirks)
    List,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCmd {
    /// List providers
    List,
    /// Add a provider
    Add(ProviderAddArgs),
    /// Remove a provider
    Remove { id: String },
    /// Send a minimal request through a provider
    Test(ProviderTestArgs),
}

#[derive(Debug, clap::Args)]
pub struct ProviderAddArgs {
    /// Built-in vendor id (see `umsp vendor list`)
    #[arg(long)]
    pub vendor: String,
    /// Instance id hint; a unique id is always assigned (vendor, vendor-2, …)
    #[arg(long)]
    pub id: Option<String>,
    /// Env variable holding the key; defaults to <ID>_API_KEY
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// Do not read a key now; set it later with `umsp env set`
    #[arg(long)]
    pub no_key: bool,
}

#[derive(Debug, clap::Args)]
pub struct ProviderTestArgs {
    pub id: String,
    /// chat or responses
    #[arg(long, default_value = "chat")]
    pub protocol: String,
}

#[derive(Debug, Subcommand)]
pub enum ModelCmd {
    /// List models
    List {
        /// Also print aliases
        #[arg(long)]
        aliases: bool,
    },
    /// Add a model
    Add(Box<ModelAddArgs>),
    /// Remove a model
    Remove { id: String },
    /// Manage a model's backends (config order is the tie-break)
    Backend {
        #[command(subcommand)]
        cmd: ModelBackendCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelBackendCmd {
    /// Append a backend (or prepend it with --first)
    Add {
        model: String,
        /// provider:upstream
        #[arg(long = "backend")]
        backend: String,
        /// Put it at the front of the tie-break order
        #[arg(long)]
        first: bool,
    },
    /// Remove every backend of one provider
    Remove {
        model: String,
        #[arg(long)]
        provider: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct ModelAddArgs {
    #[arg(long)]
    pub id: String,
    /// Repeatable backend as `provider:upstream`; order is the tie-break.
    #[arg(long = "backend", required = true, action = clap::ArgAction::Append)]
    pub backends: Vec<String>,
    /// Repeatable alias
    #[arg(long = "alias", action = clap::ArgAction::Append)]
    pub aliases: Vec<String>,
    #[arg(long)]
    pub context_length: Option<u64>,
    #[arg(long)]
    pub max_completion_tokens: Option<u64>,
    /// Repeatable capability: text, vision, tools, reasoning
    #[arg(long = "capability", action = clap::ArgAction::Append)]
    pub capabilities: Vec<String>,
    #[arg(long)]
    pub display_name: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, default_value_t = 1_789_026_056)]
    pub created: i64,
}

#[derive(Debug, Subcommand)]
pub enum ClientCmd {
    /// List clients (prefixes only, never plaintext keys)
    List,
    /// Create a client and print its key exactly once
    Add(ClientAddArgs),
    /// Delete a client
    Remove { name: String },
    /// Replace a client's key and print the new key once
    Rotate { name: String },
    /// Clear a client's device binding (next request re-binds, TOFU)
    Unbind { name: String },
}

#[derive(Debug, clap::Args)]
pub struct ClientAddArgs {
    #[arg(long)]
    pub name: String,
    /// Comma list of globs, e.g. 'od/*,deepseek/*' (default: *)
    #[arg(long)]
    pub models: Option<String>,
    /// Comma list: chat,responses,messages
    #[arg(long, default_value = "chat,responses,messages")]
    pub protocols: String,
}

#[derive(Debug, Subcommand)]
pub enum KeyCmd {
    /// Alias of `client list`
    List,
    /// Alias of `client add`
    Add(ClientAddArgs),
    /// Alias of `client remove`
    Revoke { name: String },
    /// Alias of `client unbind`
    Unbind { name: String },
}

#[derive(Debug, Subcommand)]
pub enum UsageCmd {
    /// Aggregate requests and tokens
    Summary(UsageFilterArgs),
    /// Requests/tokens grouped by client
    ByClient(UsageFilterArgs),
    /// Requests/tokens grouped by model
    ByModel(UsageFilterArgs),
    /// Requests/tokens grouped by protocol
    ByProtocol(UsageFilterArgs),
    /// Delete ledger files older than a cutoff
    Prune(PruneArgs),
}

#[derive(Debug, clap::Args)]
pub struct UsageFilterArgs {
    /// 24h | 7d | 30d | RFC3339 timestamp
    #[arg(long, default_value = "24h")]
    pub since: String,
    /// Filter by client name
    #[arg(long)]
    pub client: Option<String>,
    /// Filter by requested model id
    #[arg(long)]
    pub model: Option<String>,
    /// Filter by the provider that actually served the request
    #[arg(long)]
    pub provider: Option<String>,
    /// Filter by protocol (chat/responses/messages)
    #[arg(long)]
    pub protocol: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct PruneArgs {
    /// Delete files older than this (e.g. 30d); defaults to server.log_retention_days
    #[arg(long, conflicts_with = "days")]
    pub before: Option<String>,
    /// Keep the last N days (same as --before Nd); today's file is never removed
    #[arg(long, value_name = "N", conflicts_with = "before")]
    pub days: Option<u32>,
}

#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    #[arg(long)]
    pub provider: String,
    /// chat (M0 only)
    #[arg(long, default_value = "chat")]
    pub protocol: String,
    /// Model id to probe with (default: first model of the provider)
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct CallArgs {
    #[arg(long)]
    pub model: String,
    /// chat (M0 only)
    #[arg(long, default_value = "chat")]
    pub protocol: String,
    #[arg(long)]
    pub prompt: String,
    /// Not implemented in M0
    #[arg(long)]
    pub stream: bool,
}

#[derive(Debug, Subcommand)]
pub enum LogsCmd {
    /// Print the last N ledger lines
    Tail {
        #[arg(long, default_value_t = 20)]
        lines: usize,
        /// Keep polling for new lines
        #[arg(long)]
        follow: bool,
    },
    /// Copy ledger lines since a timestamp into one file
    Export {
        #[arg(long, default_value = "24h")]
        since: String,
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_usage_prune_days_and_rejects_both_spellings() {
        let cli = Cli::try_parse_from(["umsp", "usage", "prune", "--days", "7"]).unwrap();
        match cli.command {
            Command::Usage {
                cmd: UsageCmd::Prune(args),
            } => assert_eq!(args.days, Some(7)),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["umsp", "usage", "prune", "--days", "7", "--before", "30d"])
                .is_err()
        );
    }

    #[test]
    fn parses_env_set_and_model_backend() {
        let cli =
            Cli::try_parse_from(["umsp", "env", "set", "DEEPSEEK_API_KEY", "--stdin"]).unwrap();
        match cli.command {
            Command::Env {
                cmd: EnvCmd::Set { name, stdin },
            } => {
                assert_eq!(name, "DEEPSEEK_API_KEY");
                assert!(stdin);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "umsp",
            "model",
            "backend",
            "add",
            "deepseek-v4.1-flash",
            "--backend",
            "deepseek:deepseek-flash",
            "--first",
        ])
        .unwrap();
        match cli.command {
            Command::Model {
                cmd:
                    ModelCmd::Backend {
                        cmd:
                            ModelBackendCmd::Add {
                                model,
                                backend,
                                first,
                            },
                    },
            } => {
                assert_eq!(model, "deepseek-v4.1-flash");
                assert_eq!(backend, "deepseek:deepseek-flash");
                assert!(first);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_usage_summary_provider_filter() {
        let cli = Cli::try_parse_from([
            "umsp",
            "usage",
            "summary",
            "--provider",
            "deepseek",
            "--protocol",
            "chat",
        ])
        .unwrap();
        match cli.command {
            Command::Usage {
                cmd: UsageCmd::Summary(args),
            } => {
                assert_eq!(args.provider.as_deref(), Some("deepseek"));
                assert_eq!(args.protocol.as_deref(), Some("chat"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_nested_usage() {
        let cli =
            Cli::try_parse_from(["umsp", "usage", "summary", "--since", "7d", "--json"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Usage {
                cmd: UsageCmd::Summary(args),
            } => assert_eq!(args.since, "7d"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_model_add_with_aliases() {
        let cli = Cli::try_parse_from([
            "umsp",
            "model",
            "add",
            "--id",
            "od/m",
            "--backend",
            "open-design:m",
            "--alias",
            "a1",
            "--alias",
            "a2",
            "--capability",
            "text",
        ])
        .unwrap();
        match cli.command {
            Command::Model {
                cmd: ModelCmd::Add(args),
            } => {
                assert_eq!(args.aliases, vec!["a1", "a2"]);
                assert_eq!(args.capabilities, vec!["text"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn every_documented_subcommand_is_present() {
        // clap injects the `help` subcommand during `build()`.
        let mut command = Cli::command();
        command.build();
        let names: Vec<String> = command
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for expected in [
            "serve",
            "init",
            "config",
            "check",
            "doctor",
            "provider",
            "model",
            "catalog",
            "client",
            "key",
            "usage",
            "probe",
            "call",
            "logs",
            "login",
            "logout",
            "version",
            "completion",
            "help",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing {expected} in {names:?}"
            );
        }
    }
}
