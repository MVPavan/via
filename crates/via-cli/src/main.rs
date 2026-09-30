//! The Interface parses CLI requests and serves the VIA API; it never makes
//! session policy decisions or opens the Store.

mod client;
mod server;

use std::{io, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Args, Parser, Subcommand};
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
    /// Adds `prompt` or `prompt_file` to `params`.
    fn apply(self, params: &mut Value) -> anyhow::Result<()> {
        match (self.prompt, self.prompt_file) {
            (Some(prompt), _) => params["prompt"] = Value::String(prompt),
            (None, Some(path)) if path.as_os_str() == "-" => {
                params["prompt"] = Value::String(io::read_to_string(io::stdin())?);
            }
            (None, Some(path)) => params["prompt_file"] = json!(absolute(&path)?),
            (None, None) => anyhow::bail!("--prompt or --prompt-file is required"),
        }
        Ok(())
    }
}

/// `path` made absolute against the current directory.
fn absolute(path: &std::path::Path) -> anyhow::Result<String> {
    let path = std::path::absolute(path)?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("path is not UTF-8: {}", path.display()))
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
    fn apply(self, params: &mut Value) -> anyhow::Result<()> {
        if let Some(mode) = self.bound {
            params["bound"] =
                json!({"mode":mode,"extra_write_dirs":self.allow_dirs,"network":self.network});
        }
        if let Some(effort) = self.effort {
            params["effort"] = Value::String(effort);
        }
        if let Some(path) = self.output_schema {
            let schema = std::fs::read(&path)?;
            params["output_schema"] = serde_json::from_slice(&schema)?;
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
                    .ok_or_else(|| anyhow::anyhow!("--vendor takes harness.key=value"))?;
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

/// C1 §3.11 `via events <session|turn> [--after SEQ] [--limit N] [--types T,…]`.
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
    let code = match runtime.block_on(run(Cli::parse())) {
        Ok(code) => code,
        Err(error) => {
            let value = json!({"code": 4, "message": error.to_string(), "data": {"kind": "daemon_unreachable"}});
            let _ = write_json(io::stderr(), &value);
            4
        }
    };
    // The daemon already bounded its final shutdown; never wait here for a
    // blocking task (such as a stalled Store join) it abandoned to process exit.
    runtime.shutdown_background();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
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
            client::call("resume", &params, true, true)
        }
        Command::Steer(args) => {
            let handle = client::read_handle(
                args.handle_file.as_deref(),
                args.handle_stdin,
                args.handle.as_deref(),
                false,
            )?;
            client::call(
                "steer",
                &json!({"session":args.session,"text":args.text,"handle":handle}),
                true,
                true,
            )
        }
        Command::Cancel(args) => cancel(&args),
        Command::Close(args) => close(args),
        Command::Result(args) => {
            client::call("result", &json!({"address":args.address}), true, true)
        }
        Command::Wait(args) => wait(&args),
        Command::Events(args) => events(args),
        Command::List(args) => list(args),
        Command::Logs(args) => logs(&args.address),
        Command::Status(args) => status(&args),
        Command::Serve { .. } => client::serve_stdio().await,
    }
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
        params["instructions"] = json!({"path":absolute(&path)?});
    }
    if let Some(cwd) = args.cwd {
        params["cwd"] = json!(absolute(&cwd)?);
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
    Ok(if envelope["state"] == "completed" {
        0
    } else {
        3
    })
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
    client::call_within("wait", &params, true, true, read)
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
    client::call("events", &params, true, true)
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

/// Exit status of a foreground `spawn` or `wait` the user interrupted.
const INTERRUPTED: i32 = 130;

/// While a foreground `spawn` or `wait` waits (design §6.5): SIGINT writes
/// nothing and cancels nothing; the CLI exits 130 once stdout is flushed.
/// The daemon runs in its own process group, so the terminal's SIGINT never
/// reaches it. The handler is registered before this returns; dropping the
/// guard stops the task.
fn exit_on_interrupt() -> io::Result<Interrupt> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let task = tokio::spawn(async move {
        if interrupt.recv().await.is_some() {
            // Waits for a line being written to finish, so none is cut.
            let mut stdout = io::stdout().lock();
            let _ = io::Write::flush(&mut stdout);
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
