//! The Interface parses CLI requests and serves the VIA API; it never makes
//! session policy decisions or opens the Store.

mod client;
mod server;

use std::{io, path::PathBuf, process::ExitCode, time::Duration};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "via", version, about = "Run a coding agent through VIA")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Spawn(SpawnArgs),
    Resume(ResumeArgs),
    Steer(SteerArgs),
    Cancel(CancelArgs),
    Close(CloseArgs),
    Result(ReadArgs),
    Wait(WaitArgs),
    Events(EventsArgs),
    Logs(ReadArgs),
    Status(StatusArgs),
    List(ListArgs),
    /// The route one set of parameters would take (Task 4 design §4.6).
    Describe(DescribeArgs),
    /// The models each harness offers (Task 4 design §4.6).
    Models(ModelsArgs),
    /// Proxies C1 between stdio and the daemon socket, unchanged.
    Serve {
        #[arg(long, required = true)]
        stdio: bool,
    },
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
}

#[derive(Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent CLI switch of C1 §3.2"
)]
struct SpawnArgs {
    #[arg(long)]
    harness: String,
    #[arg(long)]
    model: String,
    #[command(flatten)]
    prompt: PromptArgs,
    /// A file whose path is sent as the session's `instructions`.
    #[arg(long)]
    instructions: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Verbs the route must support, comma separated.
    #[arg(long, value_delimiter = ',')]
    require: Vec<String>,
    #[arg(long)]
    allow_untested: bool,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    handle: Option<String>,
    #[arg(long)]
    handle_file: Option<PathBuf>,
    #[arg(long)]
    handle_stdin: bool,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[command(flatten)]
    turn: TurnArgs,
    #[arg(long)]
    background: bool,
    #[arg(long)]
    json: bool,
    /// Raw vendor arguments after `--`, sent as `vendor_args` (C1 §3.2).
    #[arg(last = true, value_name = "VENDOR_ARG")]
    vendor_args: Vec<String>,
}

#[derive(Args)]
struct DescribeArgs {
    #[arg(long)]
    harness: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    bound: Option<String>,
    #[arg(long = "allow-dir", requires = "bound")]
    allow_dirs: Vec<String>,
    #[arg(long, requires = "bound")]
    network: bool,
    /// Verbs the route must support, comma separated.
    #[arg(long, value_delimiter = ',')]
    require: Vec<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    allow_untested: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ModelsArgs {
    #[arg(long)]
    harness: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ResumeArgs {
    session: String,
    #[command(flatten)]
    prompt: PromptArgs,
    #[arg(long)]
    op_key: Option<String>,
    #[command(flatten)]
    turn: TurnArgs,
    #[arg(long)]
    handle: Option<String>,
    #[arg(long)]
    handle_file: Option<PathBuf>,
    #[arg(long)]
    handle_stdin: bool,
    #[arg(long)]
    json: bool,
    /// Sent as `vendor_args`, which the daemon refuses on resume (C1
    /// §3.3): session policy is Core's, not the CLI's.
    #[arg(last = true, hide = true)]
    vendor_args: Vec<String>,
}

/// C1 §3.2: exactly one of `--prompt` and `--prompt-file F|-`.
#[derive(Args)]
#[group(required = true, multiple = false)]
struct PromptArgs {
    #[arg(long)]
    prompt: Option<String>,
    /// Sent as `prompt_file`, made absolute; `-` reads stdin into `prompt`.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
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

/// C1 §3.2/§3.3 per-turn flags; the daemon validates them against the route.
#[derive(Args)]
struct TurnArgs {
    #[arg(long)]
    bound: Option<String>,
    #[arg(long = "allow-dir", requires = "bound")]
    allow_dirs: Vec<String>,
    #[arg(long, requires = "bound")]
    network: bool,
    #[arg(long)]
    effort: Option<String>,
    /// A file holding the JSON Schema object, or `null` to clear.
    #[arg(long)]
    output_schema: Option<PathBuf>,
    #[arg(long)]
    wall_ms: Option<u64>,
    #[arg(long)]
    idle_ms: Option<u64>,
    #[arg(long)]
    max_steps: Option<u64>,
    /// `harness.key=value`, repeatable.
    #[arg(long = "vendor")]
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
    address: String,
    #[arg(long)]
    timeout_ms: Option<u64>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SteerArgs {
    session: String,
    #[arg(long)]
    text: String,
    #[arg(long)]
    op_key: Option<String>,
    #[arg(long)]
    handle: Option<String>,
    #[arg(long)]
    handle_file: Option<PathBuf>,
    #[arg(long)]
    handle_stdin: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CancelArgs {
    session: String,
    #[arg(long)]
    turn: Option<u32>,
    #[arg(long = "force-after")]
    force_after: Option<u64>,
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    handle: Option<String>,
    #[arg(long)]
    handle_file: Option<PathBuf>,
    #[arg(long)]
    handle_stdin: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CloseArgs {
    session: String,
    #[arg(long, value_parser = ["graceful", "force"])]
    mode: Option<String>,
    #[arg(long)]
    deadline_ms: Option<u64>,
    #[arg(long)]
    op_key: Option<String>,
    #[arg(long)]
    handle: Option<String>,
    #[arg(long)]
    handle_file: Option<PathBuf>,
    #[arg(long)]
    handle_stdin: bool,
    #[arg(long)]
    json: bool,
}

/// C1 §3.7 `via status <session> [--turn N] [--after-step N] [--limit N]`.
#[derive(Args)]
struct StatusArgs {
    session: String,
    #[arg(long)]
    turn: Option<u32>,
    #[arg(long)]
    after_step: Option<u32>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long)]
    json: bool,
}

/// C1 §3.11 `via events <session|turn> [--after SEQ] [--limit N] [--types T,…]
/// [--wait-ms N] [--follow]`.
#[derive(Args)]
struct EventsArgs {
    address: String,
    #[arg(long)]
    after: Option<u64>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long, value_delimiter = ',')]
    types: Option<Vec<String>>,
    #[arg(long)]
    wait_ms: Option<u64>,
    #[arg(long)]
    follow: bool,
    #[arg(long)]
    json: bool,
}

/// C1 §3.10 `via list [--state S] [--harness H] [--label L] [--since T]
/// [--limit N] [--cursor C]`.
#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    state: Option<String>,
    #[arg(long)]
    harness: Option<String>,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReadArgs {
    address: String,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum DaemonCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        drain: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
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
            let _ = write_json(io::stderr(), &parse_error(&error).to_value());
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = write_json(
                io::stderr(),
                &json!({"code":4,"message":format!("runtime initialization failed: {error}"),"data":{"kind":"daemon_unreachable"}}),
            );
            return ExitCode::from(4);
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(error) => {
            // Refused before sending: C1's request error.
            if let Some(request) = error.downcast_ref::<client::RequestError>() {
                let _ = write_json(io::stderr(), &request.to_value());
                2
            } else {
                let value = json!({"code": 4, "message": error.to_string(), "data": {"kind": "daemon_unreachable"}});
                let _ = write_json(io::stderr(), &value);
                4
            }
        }
    };
    // The daemon already bounded its final shutdown; never wait here for a
    // blocking task (such as a stalled Store join) it abandoned to process exit.
    runtime.shutdown_background();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
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
            command: Some(DaemonCommand::Status { .. }),
        } => client::call("daemon/status", &json!({}), true, true),
        Command::Daemon {
            command: Some(DaemonCommand::Stop { drain, force, .. }),
        } => client::call(
            "daemon/stop",
            &json!({"drain": drain, "force": force}),
            // Stopping never starts a daemon.
            false,
            true,
        ),
        Command::Spawn(args) => spawn(args),
        Command::Resume(args) => {
            let handle = client::read_handle(
                args.handle_file.as_deref(),
                args.handle_stdin,
                args.handle.as_deref(),
                false,
            )?;
            let mut params = json!({"session":args.session,"handle":handle});
            args.prompt.apply(&mut params)?;
            if let Some(key) = args.op_key {
                params["op_key"] = Value::String(key);
            }
            args.turn.apply(&mut params)?;
            if !args.vendor_args.is_empty() {
                params["vendor_args"] = json!(args.vendor_args);
            }
            client::call("resume", &params, true, true)
        }
        Command::Steer(args) => {
            let handle = client::read_handle(
                args.handle_file.as_deref(),
                args.handle_stdin,
                args.handle.as_deref(),
                false,
            )?;
            let mut params = json!({"session":args.session,"text":args.text,"handle":handle});
            if let Some(key) = args.op_key {
                params["op_key"] = Value::String(key);
            }
            client::call("steer", &params, true, true)
        }
        Command::Cancel(args) => cancel(&args),
        Command::Close(args) => close(args),
        Command::Result(args) => {
            let response = client::request("result", &json!({"address":args.address}), true)?;
            let Some(envelope) = client::emit_response(&response, true)? else {
                return Ok(2);
            };
            Ok(envelope_exit_code(&envelope))
        }
        Command::Wait(args) => wait(&args),
        Command::Events(args) => events(args),
        Command::List(args) => list(args),
        Command::Logs(args) => logs(&args.address),
        Command::Status(args) => status(&args),
        Command::Describe(args) => describe(args),
        Command::Models(args) => {
            let mut params = json!({});
            if let Some(harness) = args.harness {
                params["harness"] = Value::String(harness);
            }
            client::call("models", &params, true, true)
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
    client::call("describe", &params, true, true)
}

/// `via spawn` (C1 §3.2): foreground waits for the envelope.
fn spawn(args: SpawnArgs) -> anyhow::Result<i32> {
    let handle = client::read_handle(
        args.handle_file.as_deref(),
        args.handle_stdin,
        args.handle.as_deref(),
        true,
    )?;
    let mut params = json!({"harness":args.harness,"model":args.model,"handle":handle});
    args.prompt.apply(&mut params)?;
    if let Some(path) = args.instructions {
        params["instructions"] = json!({"path":absolute(&path, "instructions")?});
    }
    if let Some(cwd) = args.cwd {
        params["cwd"] = json!(absolute(&cwd, "cwd")?);
    }
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
    let Some(receipt) = client::emit_response(&receipt, true)? else {
        return Ok(2);
    };
    if args.background {
        return Ok(0);
    }
    let address = receipt["turn"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("spawn receipt has no turn address"))?;
    let outcome = client::request("wait", &json!({"address":address}), true)?;
    let Some(envelope) = client::emit_response(&outcome, true)? else {
        return Ok(2);
    };
    Ok(envelope_exit_code(&envelope))
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
    let Some(envelope) = client::emit_response(&response, true)? else {
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
    client::call("status", &params, true, true)
}

/// `via logs` (C1 §3.12): exactly one of a session or a turn address.
fn logs(address: &str) -> anyhow::Result<i32> {
    let member = if address.contains('/') {
        "turn"
    } else {
        "session"
    };
    client::call("logs", &json!({member: address}), true, true)
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
        return follow(params, args.wait_ms);
    }
    if let Some(wait) = args.wait_ms {
        params["wait_ms"] = Value::from(wait);
    }
    client::call_within("events", &params, true, true, events_read(args.wait_ms))
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
/// each page that has events as `via events` writes a page, until Ctrl-C
/// (exit 130) or a request error (exit 2).
fn follow(mut params: Value, wait_ms: Option<u64>) -> anyhow::Result<i32> {
    let wait = wait_ms
        .filter(|wait| *wait > 0)
        .unwrap_or(via_core::EVENTS_WAIT_MAX_MS);
    params["wait_ms"] = Value::from(wait);
    let read = events_read(Some(wait));
    let _interrupt = exit_on_interrupt()?;
    loop {
        let response = client::request_within("events", &params, true, read)?;
        if response.get("error").is_some() {
            client::emit_response(&response, true)?;
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
            write_json(&mut stdout, page)?;
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
    client::call("list", &params, true, true)
}

/// `via cancel` (C1 §3.5).
fn cancel(args: &CancelArgs) -> anyhow::Result<i32> {
    let handle = client::read_handle(
        args.handle_file.as_deref(),
        args.handle_stdin,
        args.handle.as_deref(),
        false,
    )?;
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
    client::call_within("cancel", &params, true, true, read)
}

/// `via close` (C1 §3.6).
fn close(args: CloseArgs) -> anyhow::Result<i32> {
    let handle = client::read_handle(
        args.handle_file.as_deref(),
        args.handle_stdin,
        args.handle.as_deref(),
        false,
    )?;
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
    client::call_within("close", &params, true, true, read)
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

fn write_json(mut output: impl io::Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")
}

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
}
