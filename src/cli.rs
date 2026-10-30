//! Command-line interface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use std::io::IsTerminal;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::broker::{self, BrokerClient};
use crate::client::{ConnectOptions, Connection};
use crate::config::{self, Config, LOCAL, Paths};
use crate::fanout::{Outcome, Runner, Via, merge_hits, merge_sessions};
use crate::model::{self, Filters, ReadRequest, SearchRequest, SessionsRequest};
use crate::output::{self, Format, Style, emit};
use crate::pick::{self, ShellKind};
use crate::proto::{Op, Reply};
use crate::timefmt::{human_bytes, human_duration, parse_time};

#[derive(Parser, Debug)]
#[command(
    name = "sessift",
    version,
    about = "Search AI coding session history on this machine and others, live, over SSH",
    propagate_version = true
)]
pub struct Cli {
    /// Output format [default: text in a terminal, jsonl in a pipe]
    #[arg(long, global = true, value_enum)]
    pub format: Option<Format>,
    /// Print per-machine latency and transferred bytes to stderr
    #[arg(long, global = true)]
    pub timing: bool,
    /// Connect to remote machines directly instead of through the background broker
    #[arg(long, global = true)]
    pub no_broker: bool,
    /// Without a command, open the interactive picker (in a terminal).
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Search session history
    Search(SearchArgs),
    /// List sessions, most recently active first
    Sessions(SessionsArgs),
    /// Read a session page by page
    Read(ReadArgs),
    /// List machines and check that each one answers
    Machines(MachinesArgs),
    /// Diagnose connectivity, versions and latency
    Doctor(DoctorArgs),
    /// Create the config file, importing machines from memex
    Init(InitArgs),
    /// Manage sessift on remote machines
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Inspect or stop the background broker
    #[command(subcommand)]
    Broker(BrokerCmd),
    /// Build or update this machine's index of session transcripts
    Index(IndexArgs),
    /// Browse and search sessions interactively; Enter resumes one
    Pick(PickArgs),
    /// Print the key binding that opens `sessift pick` (fish, zsh or bash)
    Shell(ShellArgs),
    /// Serve the remote protocol on stdin/stdout (started over SSH)
    #[command(hide = true)]
    Serve(ServeArgs),
}

#[derive(Args, Debug, Default)]
pub struct PickArgs {
    #[command(flatten)]
    pub sel: MachineSel,
    /// Called from a shell binding: print `run`/`edit` and the command, quoted for this shell
    #[arg(long, value_enum)]
    pub shell: Option<ShellKind>,
    /// Start with this query
    #[arg(long, allow_hyphen_values = true)]
    pub query: Option<String>,
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    #[arg(value_enum)]
    pub shell: ShellKind,
    /// Key that opens the picker, in fish notation
    #[arg(long, default_value = "alt-m")]
    pub key: String,
}

#[derive(Args, Debug, Default)]
pub struct MachineSel {
    /// Machine to query; repeatable, `all` for every machine [default: [defaults].machines]
    #[arg(short = 'm', long = "machine", value_name = "ID")]
    pub machines: Vec<String>,
    /// Per-machine deadline such as 3s or 800ms [default: [defaults].timeout, 5s]
    #[arg(long, value_name = "DURATION", value_parser = duration_arg)]
    pub timeout: Option<Duration>,
}

#[derive(Args, Debug)]
pub struct FilterArgs {
    /// Only this source (claude, codex, pi, opencode, ...)
    #[arg(long)]
    pub source: Option<String>,
    /// Only this project
    #[arg(long)]
    pub project: Option<String>,
    /// Only sessions whose working directory is this path or below it
    #[arg(long)]
    pub cwd: Option<String>,
    /// Not before: 7d, 12h, 2026-10-01, 2026-10-01 09:30, RFC 3339 or Unix time
    #[arg(long, value_name = "TIME")]
    pub since: Option<String>,
    /// Not after (same formats as --since)
    #[arg(long, value_name = "TIME")]
    pub until: Option<String>,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Search terms
    #[arg(required = true, num_args = 1.., value_name = "QUERY")]
    pub query: Vec<String>,
    #[command(flatten)]
    pub sel: MachineSel,
    #[command(flatten)]
    pub filters: FilterArgs,
    /// Only records with this role (user, assistant, tool_use, tool_result)
    #[arg(long)]
    pub role: Option<String>,
    /// Only this session
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,
    /// Maximum results after merging machines
    #[arg(short = 'n', long, default_value_t = model::SEARCH_LIMIT_DEFAULT)]
    pub limit: u32,
    /// Snippet length in characters
    #[arg(long, default_value_t = model::SNIPPET_CHARS_DEFAULT)]
    pub snippet: u32,
}

#[derive(Args, Debug)]
pub struct SessionsArgs {
    #[command(flatten)]
    pub sel: MachineSel,
    #[command(flatten)]
    pub filters: FilterArgs,
    /// Only this session id
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,
    /// Maximum sessions after merging machines
    #[arg(short = 'n', long, default_value_t = model::SESSIONS_LIMIT_DEFAULT)]
    pub limit: u32,
    /// Skip this many sessions of the merged list
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
}

#[derive(Args, Debug)]
pub struct ReadArgs {
    /// Session id (from `search` or `sessions`)
    pub session_id: String,
    #[command(flatten)]
    pub sel: MachineSel,
    /// Read only this transcript path (when a session id has several)
    #[arg(long, value_name = "SOURCE_PATH")]
    pub path: Option<String>,
    /// Records to skip
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
    /// Records per page
    #[arg(short = 'n', long, default_value_t = model::READ_LIMIT_DEFAULT)]
    pub limit: u32,
    /// Character budget shared by the page
    #[arg(long, default_value_t = model::MAX_CHARS_DEFAULT)]
    pub max_chars: u32,
}

#[derive(Args, Debug)]
pub struct MachinesArgs {
    /// Per-machine deadline
    #[arg(long, value_name = "DURATION", value_parser = duration_arg)]
    pub timeout: Option<Duration>,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Machines to check [default: every machine]
    #[arg(short = 'm', long = "machine", value_name = "ID")]
    pub machines: Vec<String>,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing config file
    #[arg(long)]
    pub force: bool,
    /// memex config to import machines from [default: ~/.memex/config.toml]
    #[arg(long, value_name = "PATH")]
    pub from_memex: Option<std::path::PathBuf>,
    /// Do not import machines from memex
    #[arg(long)]
    pub no_import: bool,
}

#[derive(Subcommand, Debug)]
pub enum RemoteCmd {
    /// Copy this sessift binary to remote machines
    Install {
        /// Machines to install on (`all` for every SSH machine)
        #[arg(required = true, value_name = "ID")]
        machines: Vec<String>,
        /// Upload even if the remote binary is identical
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BrokerCmd {
    /// Show the broker and its machine connections
    Status,
    /// Stop the broker (it restarts on the next remote query)
    Stop,
    /// Run the broker in the foreground
    #[command(hide = true)]
    Run,
}

#[derive(Args, Debug)]
pub struct IndexArgs {
    #[command(subcommand)]
    pub cmd: Option<IndexCmd>,
    /// Throw the index away and build it again
    #[arg(long)]
    pub rebuild: bool,
    /// No progress output
    #[arg(long, short)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum IndexCmd {
    /// Show what the index holds and what is waiting to be indexed
    Stats,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Speak the protocol on stdin/stdout
    #[arg(long)]
    pub stdio: bool,
}

fn duration_arg(text: &str) -> Result<Duration, String> {
    config::parse_duration(text).map_err(|err| err.to_string())
}

/// Run a command; returns the process exit code.
pub async fn run(cli: Cli) -> Result<i32> {
    let paths = Paths::from_env()?;
    let command = match cli.command {
        Some(command) => command,
        None => {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                return Ok(2);
            }
            Command::Pick(PickArgs::default())
        }
    };
    match command {
        Command::Serve(args) => {
            if !args.stdio {
                bail!("only `serve --stdio` is supported");
            }
            let config = Config::load(&paths)?;
            crate::server::serve_stdio(&config).await?;
            Ok(0)
        }
        Command::Init(args) => init(&paths, args),
        Command::Shell(args) => {
            emit(&pick::shell_snippet(args.shell, &args.key)?);
            Ok(0)
        }
        Command::Broker(cmd) => broker_cmd(&paths, cmd, Format::resolve(cli.format)).await,
        Command::Index(args) => {
            let paths = paths.clone();
            let format = Format::resolve(cli.format);
            tokio::task::spawn_blocking(move || index_cmd(&paths, args, format))
                .await
                .context("index task")?
        }
        command => {
            let config = Config::load(&paths)?;
            let format = Format::resolve(cli.format);
            let runner = Arc::new(Runner::new(paths.clone(), config, !cli.no_broker));
            let ctx = Ctx {
                runner,
                format,
                timing: cli.timing,
            };
            match command {
                Command::Search(args) => search(&ctx, args).await,
                Command::Sessions(args) => sessions(&ctx, args).await,
                Command::Read(args) => read(&ctx, args).await,
                Command::Machines(args) => machines(&ctx, args).await,
                Command::Doctor(args) => doctor(&ctx, args).await,
                Command::Remote(RemoteCmd::Install { machines, force }) => {
                    remote_install(&ctx, &machines, force).await
                }
                Command::Pick(args) => pick_cmd(&ctx, args).await,
                Command::Serve(_)
                | Command::Init(_)
                | Command::Broker(_)
                | Command::Shell(_)
                | Command::Index(_) => {
                    unreachable!()
                }
            }
        }
    }
}

struct Ctx {
    runner: Arc<Runner>,
    format: Format,
    timing: bool,
}

impl Ctx {
    fn config(&self) -> &Config {
        self.runner.config()
    }

    fn timeout(&self, sel: &MachineSel) -> Duration {
        sel.timeout.unwrap_or_else(|| self.config().timeout())
    }

    /// Warnings, timing, and the exit code for a fan-out: 1 when every machine failed.
    fn finish(&self, outcomes: &[Outcome]) -> i32 {
        output::print_warnings(outcomes);
        if self.timing {
            let note = self.runner.broker_note.lock().unwrap().clone();
            output::print_timing(outcomes, note.as_deref());
        }
        if !outcomes.is_empty() && outcomes.iter().all(|o| o.result.is_err()) {
            1
        } else {
            0
        }
    }
}

impl FilterArgs {
    fn build(&self, role: Option<String>, session_id: Option<String>) -> Result<Filters> {
        let time = |value: &Option<String>, flag: &str| -> Result<Option<i64>> {
            value
                .as_deref()
                .map(|v| parse_time(v).with_context(|| format!("--{flag}")))
                .transpose()
        };
        Ok(Filters {
            source: self.source.clone(),
            project: self.project.clone(),
            cwd: self.cwd.as_deref().map(absolute_path),
            role,
            session_id,
            since_ms: time(&self.since, "since")?,
            until_ms: time(&self.until, "until")?,
        })
    }
}

fn absolute_path(path: &str) -> String {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().trim_end_matches('/').to_string();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

async fn search(ctx: &Ctx, args: SearchArgs) -> Result<i32> {
    let machines = ctx.config().select(&args.sel.machines)?;
    let request = SearchRequest {
        query: args.query.join(" "),
        limit: args.limit,
        snippet_chars: args.snippet,
        filters: args
            .filters
            .build(args.role.clone(), args.session.clone())?,
        unique_sessions: false,
    };
    request.validate().map_err(|e| anyhow!(e))?;
    let outcomes = ctx
        .runner
        .run(&machines, ctx.timeout(&args.sel), |_| {
            Op::Search(request.clone())
        })
        .await;
    let hits = merge_hits(&outcomes, request.limit as usize);
    let values = || hits.iter().map(output::hit_json).collect::<Vec<_>>();
    match ctx.format {
        Format::Text => {
            emit(&output::render_hits(&hits, &request.query));
            if hits.is_empty() && outcomes.iter().any(|o| o.result.is_ok()) {
                eprintln!("{}", Style::stderr().dim("no matches"));
            }
        }
        Format::Jsonl => emit(&output::jsonl(&values())),
        Format::Json => emit(&format!(
            "{}\n",
            json!({
                "query": request.query,
                "results": values(),
                "machines": outcomes.iter().map(output::machine_json).collect::<Vec<_>>(),
            })
        )),
    }
    Ok(ctx.finish(&outcomes))
}

async fn sessions(ctx: &Ctx, args: SessionsArgs) -> Result<i32> {
    let machines = ctx.config().select(&args.sel.machines)?;
    let filters = args.filters.build(None, args.session.clone())?;
    // One machine pages natively; several machines each return their top offset+limit.
    let request = if machines.len() == 1 {
        SessionsRequest {
            limit: args.limit,
            offset: args.offset,
            filters,
            brief: false,
        }
    } else {
        let wanted = args.offset.saturating_add(args.limit);
        if wanted > model::SESSIONS_LIMIT_MAX {
            bail!(
                "--offset + --limit must be at most {} across several machines; pick one machine with -m to page further",
                model::SESSIONS_LIMIT_MAX
            );
        }
        SessionsRequest {
            limit: wanted,
            offset: 0,
            filters,
            brief: false,
        }
    };
    request.validate().map_err(|e| anyhow!(e))?;
    let outcomes = ctx
        .runner
        .run(&machines, ctx.timeout(&args.sel), |_| {
            Op::Sessions(request.clone())
        })
        .await;
    let skip = if machines.len() == 1 {
        0
    } else {
        args.offset as usize
    };
    let merged = merge_sessions(&outcomes, skip, args.limit as usize);
    let values = || merged.iter().map(output::session_json).collect::<Vec<_>>();
    match ctx.format {
        Format::Text => {
            if merged.is_empty() {
                if outcomes.iter().any(|o| o.result.is_ok()) {
                    eprintln!("{}", Style::stderr().dim("no sessions"));
                }
            } else {
                emit(&output::render_sessions(&merged));
            }
        }
        Format::Jsonl => emit(&output::jsonl(&values())),
        Format::Json => emit(&format!(
            "{}\n",
            json!({
                "results": values(),
                "machines": outcomes.iter().map(output::machine_json).collect::<Vec<_>>(),
            })
        )),
    }
    Ok(ctx.finish(&outcomes))
}

async fn read(ctx: &Ctx, args: ReadArgs) -> Result<i32> {
    let machines = ctx.config().select(&args.sel.machines)?;
    let request = ReadRequest {
        session_id: args.session_id.clone(),
        source_path: args.path.clone(),
        offset: args.offset,
        limit: args.limit,
        max_chars: args.max_chars,
    };
    request.validate().map_err(|e| anyhow!(e))?;
    let outcomes = ctx
        .runner
        .run(&machines, ctx.timeout(&args.sel), |_| {
            Op::Read(request.clone())
        })
        .await;
    // Without -m the session is looked up on every default machine; the first in
    // configured order that knows it wins.
    let found = outcomes.iter().find_map(|o| match &o.result {
        Ok(response) => match &response.reply {
            Reply::Page(page) if page.total.unwrap_or(0) > 0 || !page.records.is_empty() => {
                Some((o.machine.clone(), page.clone()))
            }
            _ => None,
        },
        Err(_) => None,
    });
    let code = ctx.finish(&outcomes);
    let Some((machine, page)) = found else {
        if code == 0 {
            let names: Vec<&str> = outcomes.iter().map(|o| o.machine.as_str()).collect();
            eprintln!(
                "sessift: session {} not found on {}",
                args.session_id,
                names.join(", ")
            );
        }
        return Ok(1);
    };
    match ctx.format {
        Format::Text => emit(&output::render_page(&machine, &page, args.limit)),
        Format::Jsonl => {
            let mut values: Vec<_> = page.records.iter().map(output::record_json).collect();
            values.push(json!({
                "type": "page",
                "machine": machine,
                "session_id": page.session_id,
                "source_path": page.source_path,
                "offset": page.offset,
                "total": page.total,
                "next_offset": page.next_offset,
            }));
            emit(&output::jsonl(&values));
        }
        Format::Json => emit(&format!(
            "{}\n",
            json!({
                "machine": machine,
                "session_id": page.session_id,
                "source_path": page.source_path,
                "offset": page.offset,
                "total": page.total,
                "next_offset": page.next_offset,
                "records": page.records.iter().map(output::record_json).collect::<Vec<_>>(),
                "machines": outcomes.iter().map(output::machine_json).collect::<Vec<_>>(),
            })
        )),
    }
    Ok(0)
}

fn describe_transport(config: &Config, id: &str, via: Option<Via>) -> String {
    if id == LOCAL {
        return "in-process".into();
    }
    let base = config
        .spec(id)
        .map(|spec| spec.describe())
        .unwrap_or_else(|_| "?".into());
    match via {
        Some(Via::Broker) => format!("{base} (broker)"),
        Some(Via::Direct) => format!("{base} (direct)"),
        _ => base,
    }
}

async fn machines(ctx: &Ctx, args: MachinesArgs) -> Result<i32> {
    let machines = ctx.config().select(&[config::ALL.to_string()])?;
    let timeout = args.timeout.unwrap_or_else(|| ctx.config().timeout());
    let outcomes = ctx.runner.run(&machines, timeout, |_| Op::Info).await;
    match ctx.format {
        Format::Text => {
            let vias: std::collections::HashMap<&str, Via> = outcomes
                .iter()
                .map(|o| (o.machine.as_str(), o.via))
                .collect();
            emit(&output::render_machines(&outcomes, |id| {
                describe_transport(ctx.config(), id, vias.get(id).copied())
            }));
        }
        Format::Jsonl | Format::Json => {
            let values: Vec<_> = outcomes
                .iter()
                .map(|o| {
                    let mut value = output::machine_json(o);
                    value["transport"] = json!(describe_transport(ctx.config(), &o.machine, None));
                    if let Ok(response) = &o.result
                        && let Reply::Info(info) = &response.reply
                    {
                        value["info"] = serde_json::to_value(info).unwrap_or_default();
                    }
                    value
                })
                .collect();
            if ctx.format == Format::Json {
                emit(&format!("{}\n", serde_json::Value::Array(values)));
            } else {
                emit(&output::jsonl(&values));
            }
        }
    }
    if ctx.timing {
        output::print_timing(&outcomes, ctx.runner.broker_note.lock().unwrap().as_deref());
    }
    Ok(if outcomes.iter().any(|o| o.result.is_err()) {
        1
    } else {
        0
    })
}

struct Checklist {
    style: Style,
    failures: usize,
}

impl Checklist {
    fn ok(&mut self, msg: &str) {
        println!("  {} {msg}", self.style.green("✓"));
    }
    fn warn(&mut self, msg: &str) {
        println!("  {} {msg}", self.style.yellow("!"));
    }
    fn fail(&mut self, msg: &str) {
        self.failures += 1;
        println!("  {} {msg}", self.style.red("✗"));
    }
}

async fn doctor(ctx: &Ctx, args: DoctorArgs) -> Result<i32> {
    let config = ctx.config();
    let paths = ctx.runner.paths();
    let ids = if args.machines.is_empty() {
        config.select(&[config::ALL.to_string()])?
    } else {
        config.select(&args.machines)?
    };
    let mut list = Checklist {
        style: Style::stdout(),
        failures: 0,
    };
    let style = list.style;
    println!("{} {}", style.bold("sessift"), crate::VERSION);
    println!(
        "  config {}{}",
        paths.config_file().display(),
        if paths.config_file().exists() {
            ""
        } else {
            " (missing: run `sessift init`)"
        }
    );
    let sample = SearchRequest::new("error");
    for id in ids {
        println!();
        println!(
            "{} {}",
            style.bold(&id),
            style.dim(&describe_transport(config, &id, None))
        );
        if id == LOCAL {
            match ctx.runner.local_backend().await {
                Ok(backend) => {
                    let info = backend.info().await;
                    if info.version.starts_with("not found")
                        || info.version.starts_with("unavailable")
                    {
                        list.fail(&format!("backend {}: {}", info.kind, info.version));
                        continue;
                    }
                    list.ok(&format!("backend {} {}", info.kind, info.version));
                    let started = Instant::now();
                    match backend.execute(&Op::Search(sample.clone())).await {
                        Ok(Reply::Search { hits }) => list.ok(&format!(
                            "sample search: {} hits in {}ms",
                            hits.len(),
                            started.elapsed().as_millis()
                        )),
                        Ok(_) => list.fail("sample search returned an unexpected reply"),
                        Err(err) => list.fail(&format!("sample search failed: {}", err.message)),
                    }
                }
                Err(err) => list.fail(&format!("backend: {}", err.message)),
            }
            continue;
        }
        let spec = config.spec(&id)?;
        let opts = ConnectOptions {
            timeout: config.connect_timeout(),
            control_master: false,
        };
        let conn = match Connection::connect(&spec, paths, &opts).await {
            Ok(conn) => conn,
            Err(err) => {
                list.fail(&err.message);
                continue;
            }
        };
        list.ok(&format!(
            "fresh connection (handshake and server start) in {}ms",
            conn.connect_time.as_millis()
        ));
        let backend = conn
            .hello
            .backend
            .as_ref()
            .map(|b| format!("{} {}", b.kind, b.version))
            .unwrap_or_else(|| "?".into());
        let version_msg = format!(
            "remote sessift {} on {} (protocol {}), backend {backend}",
            conn.hello.version, conn.hello.host, conn.hello.protocol
        );
        if conn.hello.version == crate::VERSION {
            list.ok(&version_msg);
        } else {
            list.warn(&format!(
                "{version_msg}; this is {}: run `sessift remote install {id}`",
                crate::VERSION
            ));
        }
        if backend.contains("not found") || backend.contains("unavailable") {
            list.fail(&format!("remote backend is not usable: {backend}"));
        }
        let mut rtts = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            if conn.call(Op::Ping, Duration::from_secs(5)).await.is_ok() {
                rtts.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        if rtts.is_empty() {
            list.fail("ping failed");
            continue;
        }
        rtts.sort_by(f64::total_cmp);
        list.ok(&format!(
            "round trip: median {:.0}ms (min {:.0}ms, max {:.0}ms)",
            rtts[rtts.len() / 2],
            rtts[0],
            rtts[rtts.len() - 1]
        ));
        for (name, op) in [
            ("sample search", Op::Search(sample.clone())),
            (
                "50 sessions",
                Op::Sessions(SessionsRequest {
                    limit: 50,
                    ..SessionsRequest::default()
                }),
            ),
        ] {
            let started = Instant::now();
            match conn.call(op, Duration::from_secs(20)).await {
                Ok(response) => list.ok(&format!(
                    "{name}: {}ms total, {}ms on {id}, {} transferred",
                    started.elapsed().as_millis(),
                    response.took_ms,
                    human_bytes(response.wire_bytes)
                )),
                Err(err) => list.fail(&format!("{name} failed: {}", err.message)),
            }
        }
    }
    println!();
    println!("{}", style.bold("broker"));
    if !config.broker.enabled {
        list.warn("disabled in config ([broker] enabled = false)");
    } else {
        match BrokerClient::connect(paths).await {
            Ok(client) => match client.status().await {
                Ok(status) => {
                    list.ok(&format!(
                        "running (pid {}, up {}, {} requests)",
                        status.pid,
                        human_duration(Duration::from_millis(status.uptime_ms)),
                        status.requests
                    ));
                    if status.retiring {
                        list.warn("retiring: exits once no client is connected");
                    } else if status.build_id != config::build_id() {
                        list.warn("started by a different build; it retires on the next query");
                    }
                    for machine in status.machines {
                        if machine.connected {
                            list.ok(&format!("connected to {}", machine.id));
                        } else if let Some(err) = machine.last_error {
                            list.warn(&format!("{}: {err}", machine.id));
                        }
                    }
                }
                Err(err) => list.fail(&format!("status failed: {}", err.message)),
            },
            Err(_) => list.ok("not running (starts on the next remote query)"),
        }
    }
    Ok(if list.failures > 0 { 1 } else { 0 })
}

async fn pick_cmd(ctx: &Ctx, args: PickArgs) -> Result<i32> {
    let machines = ctx.config().select(&args.sel.machines)?;
    let options = crate::tui::TuiOptions {
        machines,
        timeout: ctx.timeout(&args.sel),
        query: args.query.unwrap_or_default(),
        shell: args.shell,
    };
    let exit = crate::tui::run(ctx.runner.clone(), options).await?;
    use crate::tui::app::Exit;
    match (exit, args.shell) {
        (Exit::Quit, _) => Ok(0),
        // A shell binding runs or edits the command itself, so it lands in history.
        (Exit::Run(command), Some(_)) => {
            emit(&format!("run\n{command}\n"));
            Ok(0)
        }
        (Exit::Edit(command), Some(_)) => {
            emit(&format!("edit\n{command}\n"));
            Ok(0)
        }
        (Exit::Run(command), None) => {
            use std::os::unix::process::CommandExt;
            eprintln!("{command}");
            let err = std::process::Command::new("/bin/sh")
                .args(["-c", &command])
                .exec();
            Err(err).context("run the resume command")
        }
        (Exit::Edit(command), None) => {
            emit(&format!("{command}\n"));
            Ok(0)
        }
    }
}

async fn remote_install(ctx: &Ctx, machines: &[String], force: bool) -> Result<i32> {
    let config = ctx.config();
    let ids: Vec<String> = config
        .select(machines)?
        .into_iter()
        .filter(|id| id != LOCAL)
        .collect();
    if ids.is_empty() {
        bail!("no remote machines selected");
    }
    let mut failed = 0;
    let mut uploaded = false;
    for id in &ids {
        match crate::install::install(ctx.runner.paths(), config, id, force).await {
            Ok(report) => uploaded |= report.uploaded,
            Err(err) => {
                failed += 1;
                eprintln!("{} {id}: {err:#}", Style::stderr().red("error:"));
            }
        }
    }
    // The broker's open connections still run the old remote binary; have it exit once idle.
    if uploaded && let Ok(client) = BrokerClient::connect(ctx.runner.paths()).await {
        client.retire().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

fn init(paths: &Paths, args: InitArgs) -> Result<i32> {
    let path = paths.config_file();
    if path.exists() && !args.force {
        println!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
        return Ok(0);
    }
    let (machines, defaults) = if args.no_import {
        (Vec::new(), Vec::new())
    } else {
        let memex = match args.from_memex {
            Some(p) => p,
            None => std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".memex/config.toml"))
                .ok_or_else(|| anyhow!("HOME is not set"))?,
        };
        match std::fs::read_to_string(&memex) {
            Ok(text) => config::import_memex_config(&text)
                .with_context(|| format!("parse {}", memex.display()))?,
            Err(_) => (Vec::new(), Vec::new()),
        }
    };
    let text = config::render_config(&machines, &defaults);
    Config::parse(&text).context("generated config is invalid")?;
    std::fs::create_dir_all(&paths.home)?;
    std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
    println!("wrote {}", path.display());
    for machine in &machines {
        println!("  machine {} → ssh {}", machine.id, machine.ssh);
    }
    if !machines.is_empty() {
        let ids: Vec<&str> = machines.iter().map(|m| m.id.as_str()).collect();
        println!("next: sessift remote install {}", ids.join(" "));
    }
    Ok(0)
}

async fn broker_cmd(paths: &Paths, cmd: BrokerCmd, format: Format) -> Result<i32> {
    match cmd {
        BrokerCmd::Run => {
            let config = Config::load(paths)?;
            broker::run(paths.clone(), config).await?;
            Ok(0)
        }
        BrokerCmd::Stop => match BrokerClient::connect(paths).await {
            Ok(client) => {
                let pid = client.hello.pid;
                client.shutdown().await;
                if broker::wait_until_gone(paths, Duration::from_secs(5)).await {
                    println!("stopped broker (pid {pid})");
                    Ok(0)
                } else {
                    eprintln!("broker (pid {pid}) did not stop");
                    Ok(1)
                }
            }
            Err(_) => {
                println!("broker is not running");
                Ok(0)
            }
        },
        BrokerCmd::Status => {
            let client = match BrokerClient::connect(paths).await {
                Ok(client) => client,
                Err(_) => {
                    if format == Format::Text {
                        println!("broker is not running (it starts on the next remote query)");
                    } else {
                        emit(&format!("{}\n", json!({"running": false})));
                    }
                    return Ok(0);
                }
            };
            let status = client.status().await.map_err(|e| anyhow!(e.message))?;
            if format != Format::Text {
                let mut value = serde_json::to_value(&status)?;
                value["running"] = json!(true);
                emit(&format!("{value}\n"));
                return Ok(0);
            }
            let style = Style::stdout();
            println!(
                "broker pid {} · {} · up {} · {} requests · {} clients",
                status.pid,
                status.version,
                human_duration(Duration::from_millis(status.uptime_ms)),
                status.requests,
                status.clients
            );
            if status.retiring {
                println!(
                    "{}",
                    style.yellow("retiring: exits once no client is connected")
                );
            } else if status.build_id != config::build_id() {
                println!(
                    "{}",
                    style.yellow("started by a different build; retires on the next query")
                );
            }
            for m in &status.machines {
                let state = if m.connected {
                    style.green("connected")
                } else {
                    style.yellow("disconnected")
                };
                let mut detail = vec![format!("{} requests", m.requests)];
                if let Some(v) = &m.remote_version {
                    detail.push(format!("remote {v}"));
                }
                if let Some(ms) = m.connected_for_ms {
                    detail.push(format!(
                        "connected {}",
                        human_duration(Duration::from_millis(ms))
                    ));
                }
                if let Some(ms) = m.idle_ms {
                    detail.push(format!(
                        "idle {}",
                        human_duration(Duration::from_millis(ms))
                    ));
                }
                if let Some(err) = &m.last_error {
                    detail.push(format!("last error: {err}"));
                }
                println!(
                    "  {} {state}  {}",
                    style.cyan(&m.id),
                    style.dim(&detail.join(" · "))
                );
            }
            Ok(0)
        }
    }
}

fn index_cmd(paths: &Paths, args: IndexArgs, format: Format) -> Result<i32> {
    use crate::index::IndexPaths;
    use crate::index::ingest::{self, Progress};
    use crate::index::source::Roots;
    use crate::index::store::Store;

    let index = IndexPaths::new(paths.index_dir());
    let roots = Roots::from_env();
    if let Some(IndexCmd::Stats) = args.cmd {
        let (pending_files, pending_bytes) = ingest::pending(&index, &roots)?;
        let store = Store::open_readonly(&index.db()).ok();
        let counts = match &store {
            Some(store) => store.counts_by_source()?,
            None => Vec::new(),
        };
        let refreshed = store
            .as_ref()
            .and_then(|s| s.meta("refreshed_ms").ok().flatten())
            .and_then(|v| v.parse::<i64>().ok());
        let size = dir_size(&index.dir);
        if format != Format::Text {
            let sources: Vec<_> = counts
                .iter()
                .map(|(source, sessions, records)| {
                    json!({"source": source, "sessions": sessions, "records": records})
                })
                .collect();
            emit(&format!(
                "{}\n",
                json!({
                    "dir": index.dir,
                    "bytes": size,
                    "refreshed_ms": refreshed,
                    "sources": sources,
                    "pending_files": pending_files,
                    "pending_bytes": pending_bytes,
                })
            ));
            return Ok(0);
        }
        let style = Style::stdout();
        println!("{} {}", style.bold("index"), index.dir.display());
        if store.is_none() {
            println!("  not built yet: run `sessift index`");
        }
        for (source, sessions, records) in &counts {
            println!("  {source:<9} {sessions:>6} sessions {records:>9} records");
        }
        println!("  size {}", human_bytes(size));
        if let Some(ms) = refreshed {
            println!("  updated {}", crate::timefmt::local_second(ms));
        }
        if pending_files > 0 {
            println!(
                "  {} files ({}) changed since; the next query or `sessift index` picks them up",
                pending_files,
                human_bytes(pending_bytes)
            );
        }
        return Ok(0);
    }

    let show = !args.quiet && std::io::stderr().is_terminal();
    let last = std::sync::Mutex::new(Instant::now());
    let progress = |p: Progress| {
        let mut last = last.lock().unwrap();
        if !show || (last.elapsed() < Duration::from_millis(200) && p.files_done < p.files_total) {
            return;
        }
        *last = Instant::now();
        eprint!(
            "\r\x1b[2Kindexing {}/{} files · {} / {}",
            p.files_done,
            p.files_total,
            human_bytes(p.bytes_done),
            human_bytes(p.bytes_total)
        );
    };
    let report = ingest::refresh(
        &index,
        &roots,
        &ingest::Options {
            wait: true,
            rebuild: args.rebuild,
            budget: None,
            in_query: false,
            progress: Some(&progress),
        },
    )?
    .expect("waits for the writer lock");
    if show {
        eprint!("\r\x1b[2K");
    }
    for (path, err) in &report.failures {
        eprintln!("{} {path}: {err}", Style::stderr().yellow("skipped"));
    }
    if format != Format::Text {
        emit(&format!(
            "{}\n",
            json!({
                "files_seen": report.files_seen,
                "files_parsed": report.files_parsed,
                "files_removed": report.files_removed,
                "files_moved": report.files_moved,
                "bytes_parsed": report.bytes_parsed,
                "records_added": report.records_added,
                "bad_lines": report.bad_lines,
                "rebuilt": report.rebuilt,
                "took_ms": report.took.as_millis() as u64,
            })
        ));
    } else if !args.quiet {
        if report.changed() {
            println!(
                "{} files parsed ({}), {} records added, {} removed, {} moved in {}",
                report.files_parsed,
                human_bytes(report.bytes_parsed),
                report.records_added,
                report.files_removed,
                report.files_moved,
                human_duration(report.took)
            );
        } else {
            println!(
                "up to date ({} files checked in {})",
                report.files_seen,
                human_duration(report.took)
            );
        }
    }
    Ok(if report.failures.is_empty() { 0 } else { 1 })
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}
