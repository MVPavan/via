//! The Interface parses CLI requests and serves the VIA API; it never makes
//! session policy decisions or opens the Store.

mod client;
mod render;
mod server;

use std::{io, path::PathBuf, process::ExitCode, time::Duration};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};

use render::{Output, View, write_json};

const TOP_EXAMPLES: &str = "\
Examples:
  via models                     the harnesses and the models each offers
  via spawn --harness <HARNESS> --model <MODEL> --prompt \"Summarize README.md\"
  via spawn --harness <HARNESS> --model <MODEL> --prompt-file task.md --background
  via wait s_7f3k9q2mzr4c        wait for the session's latest turn
  via list                       recent sessions, newest first

Output is readable text on a terminal and JSON (one object per line) when
piped or redirected; --json always prints JSON.";

const SPAWN_EXAMPLES: &str = "\
Examples:
  via spawn --harness <HARNESS> --model <MODEL> --prompt \"Fix the failing test\"
  via spawn --harness <HARNESS> --model <MODEL> --bound workspace_write \\
      --prompt-file task.md --label refactor --background > receipt.json
  echo \"Explain this repo\" | via spawn --harness <HARNESS> --model <MODEL> --prompt-file -

`via models` lists the harnesses and models. The receipt shows the
session's handle once; keep it to resume, cancel or close the session.";

const RESUME_EXAMPLES: &str = "\
Examples:
  via resume s_7f3k9q2mzr4c --handle-file handle --prompt \"Now run the tests\"
  VIA_HANDLE=h_... via resume s_7f3k9q2mzr4c --prompt-file next.md --effort high";

const WAIT_EXAMPLES: &str = "\
Examples:
  via wait s_7f3k9q2mzr4c               the session's latest turn
  via wait s_7f3k9q2mzr4c/2 --timeout-ms 900000

On timeout the turn keeps running; run wait again.";

const MODELS_EXAMPLES: &str = "\
Examples:
  via models
  via models --harness <HARNESS>";

const DESCRIBE_EXAMPLES: &str = "\
Examples:
  via describe --harness <HARNESS> --model <MODEL>
  via describe --harness <HARNESS> --model <MODEL> --bound full --network --require cancel";

#[derive(Parser)]
#[command(
    name = "via",
    version,
    about = "Run a prompt on any coding-agent harness and get a structured result",
    after_help = TOP_EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new session and run its first turn
    #[command(after_help = SPAWN_EXAMPLES)]
    Spawn(SpawnArgs),
    /// Run another turn in an existing session
    #[command(after_help = RESUME_EXAMPLES)]
    Resume(ResumeArgs),
    /// Send extra input to the running turn
    Steer(SteerArgs),
    /// Stop the running turn or a queued one
    Cancel(CancelArgs),
    /// End a session, cancelling any running or queued turns
    Close(CloseArgs),
    /// Show the result of a finished turn
    Result(ReadArgs),
    /// Wait for a turn to finish and show its result
    #[command(after_help = WAIT_EXAMPLES)]
    Wait(WaitArgs),
    /// Show a session's lifecycle events
    Events(EventsArgs),
    /// Show where a turn's transcript and evidence files are
    Logs(ReadArgs),
    /// Show a session's state and a turn's progress
    Status(StatusArgs),
    /// List sessions, newest first
    List(ListArgs),
    /// Check how a spawn would run, without starting an agent
    #[command(after_help = DESCRIBE_EXAMPLES)]
    Describe(DescribeArgs),
    /// List the models each harness offers
    #[command(after_help = MODELS_EXAMPLES)]
    Models(ModelsArgs),
    /// Relay the VIA JSON-RPC API between stdin/stdout and the daemon (for integrations)
    Serve {
        /// Use stdin and stdout
        #[arg(long, required = true)]
        stdio: bool,
    },
    /// Run the daemon in the foreground, or manage the running one
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
}

#[derive(Args)]
struct SpawnArgs {
    /// Coding-agent harness to run (`via models` lists them)
    #[arg(long)]
    harness: String,
    /// Model name or alias (see `via models`)
    #[arg(long)]
    model: String,
    #[command(flatten)]
    prompt: PromptArgs,
    /// File whose text becomes the session's system instructions
    #[arg(long, value_name = "FILE")]
    instructions: Option<PathBuf>,
    /// Working directory for the agent (default: the current directory)
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Refuse unless the route supports these verbs, comma separated (e.g. cancel,steer:partial)
    #[arg(long, value_delimiter = ',', value_name = "VERBS")]
    require: Vec<String>,
    /// Accepted for compatibility; has no effect
    #[arg(long)]
    allow_untested: bool,
    /// Label for the session, filterable in `via list`
    #[arg(long)]
    label: Option<String>,
    #[command(flatten)]
    handle: HandleArgs,
    /// Key that makes a retried spawn return the original receipt
    #[arg(long, value_name = "KEY")]
    idempotency_key: Option<String>,
    #[command(flatten)]
    turn: TurnArgs,
    /// Return after the receipt instead of waiting for the result
    #[arg(long)]
    background: bool,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
    /// Raw arguments for the vendor CLI, after `--` (unverified by VIA)
    #[arg(last = true, value_name = "VENDOR_ARG")]
    vendor_args: Vec<String>,
}

#[derive(Args)]
struct DescribeArgs {
    /// Coding-agent harness
    #[arg(long)]
    harness: Option<String>,
    /// Model name or alias
    #[arg(long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Sandbox bound: read_only, workspace_write or full"
    )]
    bound: Option<String>,
    /// Extra writable directory for the bound (repeatable)
    #[arg(long = "allow-dir", requires = "bound", value_name = "DIR")]
    allow_dirs: Vec<String>,
    /// Allow network access within the bound
    #[arg(long, requires = "bound")]
    network: bool,
    /// Verbs the route must support, comma separated
    #[arg(long, value_delimiter = ',', value_name = "VERBS")]
    require: Vec<String>,
    /// Working directory the session would use
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Accepted for compatibility; has no effect
    #[arg(long)]
    allow_untested: bool,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ModelsArgs {
    /// Only this harness's models
    #[arg(long)]
    harness: Option<String>,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ResumeArgs {
    /// Session ID (s_...)
    session: String,
    #[command(flatten)]
    prompt: PromptArgs,
    /// Key that makes a retried resume return the original receipt
    #[arg(long, value_name = "KEY")]
    op_key: Option<String>,
    #[command(flatten)]
    turn: TurnArgs,
    #[command(flatten)]
    handle: HandleArgs,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
    // Sent as `vendor_args`, which the daemon refuses on resume: vendor
    // arguments are fixed for the session's life.
    #[arg(last = true, hide = true)]
    vendor_args: Vec<String>,
}

// Exactly one of `--prompt` and `--prompt-file F|-`.
#[derive(Args)]
#[group(required = true, multiple = false)]
struct PromptArgs {
    /// The prompt text
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,
    /// Read the prompt from a file (`-` reads stdin)
    #[arg(long, value_name = "FILE")]
    prompt_file: Option<PathBuf>,
}

// The session handle: `--handle-file`, `--handle-stdin`, `VIA_HANDLE`, then
// `--handle`, in that order.
#[derive(Args)]
struct HandleArgs {
    #[arg(
        long,
        value_name = "HANDLE",
        help = "Session handle (h_...); visible to other processes, so prefer --handle-file or VIA_HANDLE"
    )]
    handle: Option<String>,
    /// Read the session handle from a file
    #[arg(long, value_name = "FILE")]
    handle_file: Option<PathBuf>,
    /// Read the session handle from stdin
    #[arg(long)]
    handle_stdin: bool,
}

impl HandleArgs {
    /// The handle from these flags or `VIA_HANDLE`; with `generate`, a new
    /// one when none is given.
    fn read(&self, generate: bool) -> anyhow::Result<String> {
        client::read_handle(
            self.handle_file.as_deref(),
            self.handle_stdin,
            self.handle.as_deref(),
            generate,
        )
    }
}

impl PromptArgs {
    /// Adds `prompt` or `prompt_file` to `params`. Every failure is a
    /// local request error (bead via-7c6).
    fn apply(self, params: &mut Value) -> Result<(), client::RequestError> {
        match (self.prompt, self.prompt_file) {
            (Some(prompt), _) => params["prompt"] = Value::String(prompt),
            (None, Some(path)) if path.as_os_str() == "-" => {
                let prompt = io::read_to_string(io::stdin()).map_err(|error| {
                    client::RequestError::invalid_params(
                        "prompt",
                        format!("reading the prompt from stdin: {error}"),
                    )
                })?;
                params["prompt"] = Value::String(prompt);
            }
            (None, Some(path)) => params["prompt_file"] = json!(absolute(&path, "prompt_file")?),
            (None, None) => {
                return Err(client::RequestError::invalid_params(
                    "prompt",
                    "--prompt or --prompt-file is required",
                ));
            }
        }
        Ok(())
    }
}

/// `path` made absolute against the current directory; a failure is a
/// local request error naming `field` (bead via-7c6).
fn absolute(path: &std::path::Path, field: &'static str) -> Result<String, client::RequestError> {
    let path = std::path::absolute(path)
        .map_err(|error| client::RequestError::invalid_params(field, error.to_string()))?;
    path.to_str().map(str::to_owned).ok_or_else(|| {
        client::RequestError::invalid_params(
            field,
            format!("path is not UTF-8: {}", path.display()),
        )
    })
}

// Per-turn settings, inherited by later turns unless changed; the daemon
// validates them against the route.
#[derive(Args)]
struct TurnArgs {
    #[arg(
        long,
        value_name = "MODE",
        help = "Sandbox bound: read_only, workspace_write or full"
    )]
    bound: Option<String>,
    /// Extra writable directory for the bound (repeatable)
    #[arg(long = "allow-dir", requires = "bound", value_name = "DIR")]
    allow_dirs: Vec<String>,
    /// Allow network access within the bound
    #[arg(long, requires = "bound")]
    network: bool,
    /// Reasoning effort: low ... max, or a vendor value
    #[arg(long)]
    effort: Option<String>,
    /// File holding a JSON Schema the final answer must match (`null` clears it)
    #[arg(long, value_name = "FILE")]
    output_schema: Option<PathBuf>,
    /// Wall-clock limit for the turn, in milliseconds (default 3600000)
    #[arg(long, value_name = "MS")]
    wall_ms: Option<u64>,
    /// Limit on time without agent activity, in milliseconds (default 600000)
    #[arg(long, value_name = "MS")]
    idle_ms: Option<u64>,
    /// Maximum model steps for the turn
    #[arg(long, value_name = "N")]
    max_steps: Option<u64>,
    /// Vendor option as harness.key=value (repeatable)
    #[arg(long = "vendor", value_name = "HARNESS.KEY=VALUE")]
    vendor: Vec<String>,
}

impl TurnArgs {
    /// Adds the given per-turn parameters to `params`; omitted ones inherit.
    /// Every failure is a local request error naming its member (bead
    /// via-7c6): nothing is sent.
    fn apply(self, params: &mut Value) -> Result<(), client::RequestError> {
        if let Some(mode) = self.bound {
            params["bound"] =
                json!({"mode":mode,"extra_write_dirs":self.allow_dirs,"network":self.network});
        }
        if let Some(effort) = self.effort {
            params["effort"] = Value::String(effort);
        }
        if let Some(path) = self.output_schema {
            let invalid = |error: &dyn std::fmt::Display| {
                client::RequestError::invalid_params(
                    "output_schema",
                    format!("--output-schema {}: {error}", path.display()),
                )
            };
            let schema = std::fs::read(&path).map_err(|error| invalid(&error))?;
            params["output_schema"] =
                serde_json::from_slice(&schema).map_err(|error| invalid(&error))?;
        }
        if self.wall_ms.is_some() || self.idle_ms.is_some() {
            let mut deadlines = json!({});
            if let Some(wall) = self.wall_ms {
                deadlines["wall_ms"] = Value::from(wall);
            }
            if let Some(idle) = self.idle_ms {
                deadlines["idle_ms"] = Value::from(idle);
            }
            params["deadlines"] = deadlines;
        }
        if let Some(steps) = self.max_steps {
            params["max_steps"] = Value::from(steps);
        }
        if !self.vendor.is_empty() {
            let mut vendor = json!({});
            for option in self.vendor {
                let (name, value) = option
                    .split_once('=')
                    .and_then(|(key, value)| Some((key.split_once('.')?, value)))
                    .filter(|((harness, key), _)| !harness.is_empty() && !key.is_empty())
                    .ok_or_else(|| {
                        client::RequestError::invalid_params(
                            "vendor",
                            "--vendor takes harness.key=value",
                        )
                    })?;
                vendor[name.0][name.1] = Value::String(value.to_owned());
            }
            params["vendor"] = vendor;
        }
        Ok(())
    }
}

#[derive(Args)]
struct WaitArgs {
    /// Turn address (s_.../N) or session ID (its latest turn)
    address: String,
    /// Give up after this many milliseconds (default 30000); the turn keeps running
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SteerArgs {
    /// Session ID (s_...)
    session: String,
    /// Input to send to the running turn
    #[arg(long)]
    text: String,
    /// Key that makes a retried steer return the original outcome
    #[arg(long, value_name = "KEY")]
    op_key: Option<String>,
    #[command(flatten)]
    handle: HandleArgs,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CancelArgs {
    /// Session ID (s_...)
    session: String,
    /// Turn number to cancel (default: the running turn, else the latest)
    #[arg(long, value_name = "N")]
    turn: Option<u32>,
    /// Milliseconds the agent gets to stop before VIA forces it (default 10000)
    #[arg(long = "force-after", value_name = "MS")]
    force_after: Option<u64>,
    /// Reply only once the turn has stopped
    #[arg(long)]
    wait: bool,
    #[command(flatten)]
    handle: HandleArgs,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CloseArgs {
    /// Session ID (s_...)
    session: String,
    /// graceful lets the agent stop on its own; force stops running work at once
    #[arg(long, value_parser = ["graceful", "force"])]
    mode: Option<String>,
    /// How long a graceful close may take, in milliseconds
    #[arg(long, value_name = "MS")]
    deadline_ms: Option<u64>,
    /// Key that makes a retried close return the original receipt
    #[arg(long, value_name = "KEY")]
    op_key: Option<String>,
    #[command(flatten)]
    handle: HandleArgs,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatusArgs {
    /// Session ID (s_...)
    session: String,
    /// Turn number to describe (default: the running turn, else the latest)
    #[arg(long, value_name = "N")]
    turn: Option<u32>,
    /// Show steps after this step number
    #[arg(long, value_name = "N")]
    after_step: Option<u32>,
    /// Maximum steps to show (default 100, max 1000)
    #[arg(long, value_name = "N")]
    limit: Option<u32>,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct EventsArgs {
    /// Session ID (s_...) or turn address (s_.../N)
    address: String,
    /// Show events after this sequence number
    #[arg(long, value_name = "SEQ")]
    after: Option<u64>,
    /// Maximum events to show (default 200, max 1000)
    #[arg(long, value_name = "N")]
    limit: Option<u32>,
    /// Only these event types, comma separated (e.g. turn.ended,warning)
    #[arg(long, value_delimiter = ',', value_name = "TYPES")]
    types: Option<Vec<String>>,
    /// Wait up to this many milliseconds for new events (max 30000)
    #[arg(long, value_name = "MS")]
    wait_ms: Option<u64>,
    /// Keep printing new events until Ctrl-C
    #[arg(long)]
    follow: bool,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Only sessions in this state: idle, active or closed
    #[arg(long)]
    state: Option<String>,
    /// Only sessions of this harness
    #[arg(long)]
    harness: Option<String>,
    /// Only sessions with this label
    #[arg(long)]
    label: Option<String>,
    /// Only sessions active since this RFC 3339 time (e.g. 2026-10-01T00:00:00Z)
    #[arg(long, value_name = "TIME")]
    since: Option<String>,
    /// Maximum sessions to show
    #[arg(long, value_name = "N")]
    limit: Option<u32>,
    /// Continue from a previous page's next cursor
    #[arg(long)]
    cursor: Option<String>,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReadArgs {
    /// Turn address (s_.../N) or session ID (its latest turn)
    address: String,
    /// Print JSON even on a terminal
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Show the daemon's version, sessions, health and storage
    Status {
        /// Print JSON even on a terminal
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon (refused while sessions are busy unless --drain or --force)
    Stop {
        /// Refuse new work, let accepted turns finish, then stop
        #[arg(long)]
        drain: bool,
        /// Force-close every session with unfinished work, then stop
        #[arg(long)]
        force: bool,
        /// Print JSON even on a terminal
        #[arg(long)]
        json: bool,
    },
}

impl Command {
    /// Whether `--json` was given. The entry points that never print a
    /// result for people (`daemon`, `serve --stdio`) always use JSON.
    fn json(&self) -> bool {
        match self {
            Self::Spawn(args) => args.json,
            Self::Resume(args) => args.json,
            Self::Steer(args) => args.json,
            Self::Cancel(args) => args.json,
            Self::Close(args) => args.json,
            Self::Result(args) | Self::Logs(args) => args.json,
            Self::Wait(args) => args.json,
            Self::Events(args) => args.json,
            Self::Status(args) => args.json,
            Self::List(args) => args.json,
            Self::Describe(args) => args.json,
            Self::Models(args) => args.json,
            Self::Daemon {
                command: Some(DaemonCommand::Status { json } | DaemonCommand::Stop { json, .. }),
            } => *json,
            Self::Serve { .. } | Self::Daemon { command: None } => true,
        }
    }
}

fn main() -> ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__via_host_anchor")) {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        return ExitCode::from(u8::try_from(via_core::run_anchor_from_args(&args)).unwrap_or(1));
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__via_host_exec")) {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        return ExitCode::from(u8::try_from(via_core::run_exec_from_args(&args)).unwrap_or(125));
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // Help and version keep the parser's own output and exit code.
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                error.exit();
            }
            // On a terminal, the parser's own message with its usage and tips.
            if !render::wants_json(json_requested(), io::IsTerminal::is_terminal(&io::stderr())) {
                // Safe to ignore: a failed write to stderr has no other outlet.
                let _ = error.print();
                return ExitCode::from(2);
            }
            let _ = write_json(io::stderr(), &parse_error(&error).to_value());
            return ExitCode::from(2);
        }
    };
    let json = cli.command.json();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = render::error(
                &json!({"code":4,"message":format!("runtime initialization failed: {error}"),"data":{"kind":"daemon_unreachable"}}),
                json,
            );
            return ExitCode::from(4);
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(error) => {
            // Refused before sending: C1's request error.
            if let Some(request) = error.downcast_ref::<client::RequestError>() {
                let _ = render::error(&request.to_value(), json);
                2
            } else {
                let value = json!({"code": 4, "message": error.to_string(), "data": {"kind": "daemon_unreachable"}});
                let _ = render::error(&value, json);
                4
            }
        }
    };
    // The daemon already bounded its final shutdown; never wait here for a
    // blocking task (such as a stalled Store join) it abandoned to process exit.
    runtime.shutdown_background();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Whether `--json` is among the arguments before any `--`: the request
/// for JSON when the parser refused the command line.
fn json_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
}

/// An argument the parser rejected, as C1's request error (review
/// clfix-crit): `invalid_params` naming the CLI argument in the CLI's own
/// spelling (review clfix-crit2), never a C1 member: see [`cli_field`].
/// The message is the parser's own, without its usage and tips.
fn parse_error(error: &clap::Error) -> client::RequestError {
    // An unknown argument is never looked up: its token may match a flag
    // another verb defines, or be a value after `--` (review clfix-crit4).
    let named = match error.get(ContextKind::InvalidArg) {
        _ if error.kind() == clap::error::ErrorKind::UnknownArgument => None,
        Some(ContextValue::String(arg)) => Some(arg.as_str()),
        Some(ContextValue::Strings(args)) => args.first().map(String::as_str),
        _ => None,
    };
    let field = named.map_or_else(|| "command".to_owned(), cli_field);
    let text = error.to_string();
    let text = text.strip_prefix("error: ").unwrap_or(&text);
    let message = text
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    client::RequestError::invalid_params(field, message)
}

/// The CLI argument the parser shows as `shown`, read from the parser's
/// own definitions rather than its display text: a flag as `--name`, a
/// positional by its lowercase name, a required group as its member flags
/// joined by `|`. Anything the definitions do not name (an unknown flag,
/// long or short, a stray value, a value after `--`, an unknown verb) is
/// `command`: a typed token is never echoed as a field (review
/// clfix-crit3).
fn cli_field(shown: &str) -> String {
    let field = |arg: &clap::Arg| {
        arg.get_long().map_or_else(
            || arg.get_id().as_str().to_ascii_lowercase(),
            |long| format!("--{long}"),
        )
    };
    // Built, as the parser was: an argument displays only once built.
    let mut root = Cli::command();
    root.build();
    let mut commands = vec![&root];
    let mut all = Vec::new();
    while let Some(command) = commands.pop() {
        all.push(command);
        commands.extend(command.get_subcommands());
    }
    for command in &all {
        if let Some(arg) = command.get_arguments().find(|arg| arg.to_string() == shown) {
            return field(arg);
        }
    }
    for command in &all {
        for group in command.get_groups() {
            let members: Vec<&clap::Arg> = group
                .get_args()
                .filter_map(|id| command.get_arguments().find(|arg| arg.get_id() == id))
                .collect();
            if members.len() > 1
                && group.is_required_set()
                && members
                    .iter()
                    .all(|arg| shown.contains(arg.to_string().as_str()))
            {
                return members
                    .iter()
                    .map(|arg| field(arg))
                    .collect::<Vec<_>>()
                    .join("|");
            }
        }
    }
    "command".to_owned()
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Daemon { command: None } => server::serve().await,
        Command::Daemon {
            command: Some(DaemonCommand::Status { json }),
        } => client::call(
            "daemon/status",
            &json!({}),
            true,
            Output::new(View::DaemonStatus, json),
        ),
        Command::Daemon {
            command: Some(DaemonCommand::Stop { drain, force, json }),
        } => client::call(
            "daemon/stop",
            &json!({"drain": drain, "force": force}),
            // Stopping never starts a daemon.
            false,
            Output::new(View::DaemonStop, json),
        ),
        Command::Spawn(args) => spawn(args),
        Command::Resume(args) => {
            let handle = args.handle.read(false)?;
            let mut params = json!({"session":args.session,"handle":handle});
            args.prompt.apply(&mut params)?;
            if let Some(key) = args.op_key {
                params["op_key"] = Value::String(key);
            }
            args.turn.apply(&mut params)?;
            if !args.vendor_args.is_empty() {
                params["vendor_args"] = json!(args.vendor_args);
            }
            client::call(
                "resume",
                &params,
                true,
                Output::new(View::Resumed, args.json),
            )
        }
        Command::Steer(args) => {
            let handle = args.handle.read(false)?;
            let mut params = json!({"session":args.session,"text":args.text,"handle":handle});
            if let Some(key) = args.op_key {
                params["op_key"] = Value::String(key);
            }
            client::call("steer", &params, true, Output::new(View::Other, args.json))
        }
        Command::Cancel(args) => cancel(&args),
        Command::Close(args) => close(args),
        Command::Result(args) => {
            let response = client::request("result", &json!({"address":args.address}), true)?;
            let output = Output::new(View::Envelope, args.json);
            let Some(envelope) = client::emit_response(&response, output)? else {
                return Ok(2);
            };
            Ok(envelope_exit_code(&envelope))
        }
        Command::Wait(args) => wait(&args),
        Command::Events(args) => events(args),
        Command::List(args) => list(args),
        Command::Logs(args) => logs(&args.address, args.json),
        Command::Status(args) => status(&args),
        Command::Describe(args) => describe(args),
        Command::Models(args) => {
            let mut params = json!({});
            if let Some(harness) = args.harness {
                params["harness"] = Value::String(harness);
            }
            client::call(
                "models",
                &params,
                true,
                Output::new(View::Models, args.json),
            )
        }
        Command::Serve { .. } => client::serve_stdio().await,
    }
}

/// `via describe` (Task 4 design §4.6): writes nothing.
fn describe(args: DescribeArgs) -> anyhow::Result<i32> {
    let mut params = json!({});
    if let Some(harness) = args.harness {
        params["harness"] = Value::String(harness);
    }
    if let Some(model) = args.model {
        params["model"] = Value::String(model);
    }
    if let Some(mode) = args.bound {
        params["bound"] =
            json!({"mode":mode,"extra_write_dirs":args.allow_dirs,"network":args.network});
    }
    if !args.require.is_empty() {
        params["require"] = json!(args.require);
    }
    if let Some(cwd) = args.cwd {
        params["cwd"] = json!(absolute(&cwd, "cwd")?);
    }
    if args.allow_untested {
        params["allow_untested"] = Value::Bool(true);
    }
    client::call(
        "describe",
        &params,
        true,
        Output::new(View::Describe, args.json),
    )
}

/// `via spawn` (C1 §3.2): foreground waits for the envelope.
fn spawn(args: SpawnArgs) -> anyhow::Result<i32> {
    let handle = args.handle.read(true)?;
    let mut params = json!({"harness":args.harness,"model":args.model,"handle":handle});
    args.prompt.apply(&mut params)?;
    if let Some(path) = args.instructions {
        params["instructions"] = json!({"path":absolute(&path, "instructions")?});
    }
    // A new session without `--cwd` runs in the caller's directory, not
    // the daemon's.
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .map_err(|error| client::RequestError::invalid_params("cwd", error.to_string()))?,
    };
    params["cwd"] = json!(absolute(&cwd, "cwd")?);
    if !args.require.is_empty() {
        params["require"] = json!(args.require);
    }
    if args.allow_untested {
        params["allow_untested"] = Value::Bool(true);
    }
    if let Some(label) = args.label {
        params["label"] = Value::String(label);
    }
    if let Some(key) = args.idempotency_key {
        params["idempotency_key"] = Value::String(key);
    }
    args.turn.apply(&mut params)?;
    if !args.vendor_args.is_empty() {
        params["vendor_args"] = json!(args.vendor_args);
    }
    // Foreground: Ctrl-C leaves the turn running (design §6.5).
    let _interrupt = (!args.background).then(exit_on_interrupt).transpose()?;
    let mut receipt = client::request("spawn", &params, true)?;
    if let Some(result) = receipt.get_mut("result").and_then(Value::as_object_mut) {
        result.insert("handle".to_owned(), Value::String(handle));
    }
    let Some(receipt) = client::emit_response(&receipt, Output::new(View::Receipt, args.json))?
    else {
        return Ok(2);
    };
    if args.background {
        return Ok(0);
    }
    let address = receipt["turn"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("spawn receipt has no turn address"))?;
    // C1 P1: wait until the turn is terminal. Each `wait` is bounded, and
    // the read with it; one that times out while the turn runs is resent.
    let bound = spawn_wait_ms();
    let read = Duration::from_millis(bound).saturating_add(Duration::from_secs(5));
    let wait = json!({"address":address,"timeout_ms":bound});
    let outcome = loop {
        let outcome = client::request_within("wait", &wait, true, read)?;
        if outcome["error"]["data"]["kind"] != "wait_timeout" {
            break outcome;
        }
    };
    if !render::wants_json(args.json, io::IsTerminal::is_terminal(&io::stdout())) {
        // Text: a blank line between the receipt and the result.
        io::Write::write_all(&mut io::stdout().lock(), b"\n")?;
    }
    let output = Output::new(View::Envelope, args.json);
    let Some(envelope) = client::emit_response(&outcome, output)? else {
        return Ok(2);
    };
    Ok(envelope_exit_code(&envelope))
}

/// The bound of each `wait` a foreground `spawn` sends: the daemon's
/// default; test builds only may lower it with `VIA_TEST_SPAWN_WAIT_MS`.
fn spawn_wait_ms() -> u64 {
    #[cfg(feature = "test-failpoints")]
    if let Some(bound) = std::env::var("VIA_TEST_SPAWN_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return bound;
    }
    via_core::DEFAULT_WAIT_MS
}

/// `via wait` (C1 §3.8).
fn wait(args: &WaitArgs) -> anyhow::Result<i32> {
    let mut params = json!({"address":args.address});
    let mut read = Duration::from_millis(via_core::DEFAULT_WAIT_MS);
    if let Some(timeout) = args.timeout_ms {
        params["timeout_ms"] = Value::from(timeout);
        read = Duration::from_millis(timeout);
    }
    // The daemon answers `wait_timeout` at the bound; allow for the reply.
    let read = read.saturating_add(Duration::from_secs(5));
    let _interrupt = exit_on_interrupt()?;
    let response = client::request_within("wait", &params, true, read)?;
    let output = Output::new(View::Envelope, args.json);
    let Some(envelope) = client::emit_response(&response, output)? else {
        return Ok(2);
    };
    Ok(envelope_exit_code(&envelope))
}

/// C1 §1: foreground spawn, wait and result share the terminal-envelope exit rule.
fn envelope_exit_code(envelope: &Value) -> i32 {
    match envelope["state"].as_str() {
        Some("failed" | "cancelled" | "unknown") => 3,
        _ => 0,
    }
}

/// `via status` (C1 §3.7): the omitted members take the daemon's defaults.
fn status(args: &StatusArgs) -> anyhow::Result<i32> {
    let mut params = json!({"session":args.session});
    for (member, value) in [
        ("turn", args.turn),
        ("after_step", args.after_step),
        ("limit", args.limit),
    ] {
        if let Some(value) = value {
            params[member] = Value::from(value);
        }
    }
    client::call(
        "status",
        &params,
        true,
        Output::new(View::Status, args.json),
    )
}

/// `via logs` (C1 §3.12): exactly one of a session or a turn address.
fn logs(address: &str, json: bool) -> anyhow::Result<i32> {
    let member = if address.contains('/') {
        "turn"
    } else {
        "session"
    };
    client::call(
        "logs",
        &json!({member: address}),
        true,
        Output::new(View::Logs, json),
    )
}

/// `via events` (C1 §3.11): a session or a turn address, and the page's
/// window and filter.
fn events(args: EventsArgs) -> anyhow::Result<i32> {
    let member = if args.address.contains('/') {
        "turn"
    } else {
        "session"
    };
    let mut params = json!({member: args.address});
    if let Some(after) = args.after {
        params["after"] = Value::from(after);
    }
    if let Some(limit) = args.limit {
        params["limit"] = Value::from(limit);
    }
    if let Some(types) = args.types {
        params["types"] = Value::from(types);
    }
    if args.follow {
        return follow(params, args.wait_ms, args.json);
    }
    if let Some(wait) = args.wait_ms {
        params["wait_ms"] = Value::from(wait);
    }
    client::call_within(
        "events",
        &params,
        true,
        Output::new(View::Events, args.json),
        events_read(args.wait_ms),
    )
}

/// Read bound of an `events` call: the daemon replies by `wait_ms`; allow
/// 5 s for the reply, and never less than a plain request's 30 s.
fn events_read(wait_ms: Option<u64>) -> Duration {
    Duration::from_millis(wait_ms.unwrap_or(0))
        .saturating_add(Duration::from_secs(5))
        .max(Duration::from_secs(30))
}

/// `via events --follow` (C1 §3.11): long-polls from each reply's
/// `next_after` (`--wait-ms`, else the longest bound, per poll) and writes
/// each page that has events as `via events` writes a page (as text rows
/// on a terminal), until Ctrl-C (exit 130) or a request error (exit 2).
fn follow(mut params: Value, wait_ms: Option<u64>, json: bool) -> anyhow::Result<i32> {
    let wait = wait_ms
        .filter(|wait| *wait > 0)
        .unwrap_or(via_core::EVENTS_WAIT_MAX_MS);
    params["wait_ms"] = Value::from(wait);
    let read = events_read(Some(wait));
    let as_json = render::wants_json(json, io::IsTerminal::is_terminal(&io::stdout()));
    let _interrupt = exit_on_interrupt()?;
    loop {
        let response = client::request_within("events", &params, true, read)?;
        if response.get("error").is_some() {
            client::emit_response(&response, Output::new(View::Events, json))?;
            return Ok(2);
        }
        let page = response
            .get("result")
            .ok_or_else(|| anyhow::anyhow!("daemon reply has no result"))?;
        if page["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
        {
            // One lock through the page and its newline: the Ctrl-C handler
            // waits up to `INTERRUPT_FLUSH` for the same lock before it
            // exits, so a page a reader drains is never cut; one a stalled
            // reader leaves blocked may end unfinished (C1 §3.11).
            let mut stdout = io::stdout().lock();
            if as_json {
                write_json(&mut stdout, page)?;
            } else {
                io::Write::write_all(&mut stdout, render::event_rows(page).as_bytes())?;
            }
            io::Write::flush(&mut stdout)?;
        }
        params["after"] = page["next_after"].clone();
    }
}

/// `via list` (C1 §3.10): one page of session summaries.
fn list(args: ListArgs) -> anyhow::Result<i32> {
    let mut params = json!({});
    for (member, value) in [
        ("state", args.state),
        ("harness", args.harness),
        ("label", args.label),
        ("since", args.since),
        ("cursor", args.cursor),
    ] {
        if let Some(value) = value {
            params[member] = Value::from(value);
        }
    }
    if let Some(limit) = args.limit {
        params["limit"] = Value::from(limit);
    }
    client::call("list", &params, true, Output::new(View::List, args.json))
}

/// `via cancel` (C1 §3.5).
fn cancel(args: &CancelArgs) -> anyhow::Result<i32> {
    let handle = args.handle.read(false)?;
    let mut params = json!({"session":args.session,"handle":handle});
    if let Some(turn) = args.turn {
        params["turn"] = Value::from(turn);
    }
    if let Some(force_after) = args.force_after {
        params["force_after_ms"] = Value::from(force_after);
    }
    // With `--wait` the reply comes once the turn is terminal, which
    // its own deadlines bound.
    let read = if args.wait {
        params["wait"] = Value::Bool(true);
        CANCEL_WAIT_READ
    } else {
        Duration::from_secs(30)
    };
    client::call_within(
        "cancel",
        &params,
        true,
        Output::new(View::Cancelled, args.json),
        read,
    )
}

/// `via close` (C1 §3.6).
fn close(args: CloseArgs) -> anyhow::Result<i32> {
    let handle = args.handle.read(false)?;
    let mut params = json!({"session":args.session,"handle":handle});
    if let Some(mode) = args.mode {
        params["mode"] = Value::String(mode);
    }
    let mut read = Duration::from_millis(via_core::DEFAULT_CLOSE_DEADLINE_MS);
    if let Some(deadline) = args.deadline_ms {
        params["deadline_ms"] = Value::from(deadline);
        read = Duration::from_millis(deadline);
    }
    if let Some(key) = args.op_key {
        params["op_key"] = Value::String(key);
    }
    // The close replies once settled, within its deadline plus the
    // time a running turn's terminal takes.
    let read = read.saturating_add(Duration::from_secs(30));
    client::call_within(
        "close",
        &params,
        true,
        Output::new(View::Closed, args.json),
        read,
    )
}

/// Exit status of a foreground `spawn`, `wait` or `events --follow` the
/// user interrupted.
const INTERRUPTED: i32 = 130;

/// How long an interrupted CLI waits for stdout before it exits anyway.
const INTERRUPT_FLUSH: Duration = Duration::from_millis(500);

/// While a foreground `spawn`, `wait` or `events --follow` waits (design
/// §6.5): SIGINT writes nothing and cancels nothing; the CLI exits 130 once
/// stdout is flushed, or after `INTERRUPT_FLUSH` if stdout is stalled. The
/// daemon runs in its own process group, so the terminal's SIGINT never
/// reaches it. The handler is registered before this returns; dropping the
/// guard stops the task.
fn exit_on_interrupt() -> io::Result<Interrupt> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let task = tokio::spawn(async move {
        if interrupt.recv().await.is_some() {
            // A helper takes the stdout lock, so a line being written
            // finishes first and none starts after; it keeps the lock until
            // the exit. A write the reader does not take within
            // `INTERRUPT_FLUSH` (stopped or slow) keeps the lock: the CLI
            // then exits without it, the last line possibly unfinished. The exit itself never waits
            // for the lock (std's exit-time stdout cleanup only tries it).
            let (flushed, done) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let mut stdout = io::stdout().lock();
                let _ = io::Write::flush(&mut stdout);
                let _ = flushed.send(());
                loop {
                    std::thread::park();
                }
            });
            let _ = tokio::time::timeout(INTERRUPT_FLUSH, done).await;
            std::process::exit(INTERRUPTED);
        }
    });
    Ok(Interrupt(task))
}

/// Owns the interrupt task for as long as the CLI waits.
struct Interrupt(tokio::task::JoinHandle<()>);

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Read bound of `cancel --wait`, whose reply waits for the turn's terminal.
const CANCEL_WAIT_READ: Duration = Duration::from_hours(24);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_exit_three_covers_exactly_c1_failed_cancelled_unknown() {
        for state in ["failed", "cancelled", "unknown"] {
            assert_eq!(envelope_exit_code(&json!({"state": state})), 3, "{state}");
        }
        for state in ["completed", "queued", "running", "future-state"] {
            assert_eq!(envelope_exit_code(&json!({"state": state})), 0, "{state}");
        }
        assert_eq!(envelope_exit_code(&json!({})), 0);
    }

    /// Every command and every visible argument has a description, and no
    /// help text cites a specification.
    #[test]
    fn help_describes_everything_in_plain_language() {
        let mut root = Cli::command();
        root.build();
        let mut commands = vec![(String::from("via"), root)];
        let mut problems = Vec::new();
        while let Some((path, mut command)) = commands.pop() {
            if command.get_about().is_none() {
                problems.push(format!("{path}: no description"));
            }
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                if !arg.is_hide_set()
                    && arg.get_help().is_none()
                    && !matches!(id, "help" | "version")
                {
                    problems.push(format!("{path} {id}: no description"));
                }
            }
            let help = command.render_long_help().to_string();
            for cited in ["C1", "§", "Task ", "design"] {
                if help.contains(cited) {
                    problems.push(format!("{path}: help cites {cited:?}"));
                }
            }
            for sub in command.get_subcommands() {
                if sub.get_name() != "help" {
                    commands.push((format!("{path} {}", sub.get_name()), sub.clone()));
                }
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }
}
