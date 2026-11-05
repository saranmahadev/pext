//! The `pext` command line. The clap command tree is generated from the
//! engine's operation registry, so the CLI, MCP tools and HTTP API can never
//! drift apart.

#![allow(clippy::result_large_err)]

use chrono::NaiveDate;
use clap::{Arg, ArgAction, ArgMatches, Command};
use pext_core::{Error, ErrorKind, Result};
use pext_engine::registry::{self, Kind, OpSpec, GROUPS};
use pext_engine::{Actor, CallOpts, Engine, EngineConfig};
use pext_render::{Format, Opts};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// Everything environmental, injectable for tests.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub is_tty: bool,
    pub no_color_env: bool,
    pub home: Option<PathBuf>,
    pub db: Option<String>,
    pub token: Option<String>,
    pub output: Option<String>,
    pub today: Option<String>,
}

impl Env {
    pub fn from_process() -> Env {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Env {
            is_tty: std::io::stdout().is_terminal(),
            no_color_env: std::env::var_os("NO_COLOR").is_some(),
            home: var("HOME")
                .or_else(|| var("USERPROFILE"))
                .map(PathBuf::from),
            db: var("PEXT_DB"),
            token: var("PEXT_TOKEN"),
            output: var("PEXT_OUTPUT"),
            today: var("PEXT_TODAY"),
        }
    }
}

const GLOBALS: &[&str] = &[
    "db",
    "output",
    "fields",
    "pretty",
    "quiet",
    "no_color",
    "dry_run",
    "yes",
    "idempotency_key",
    "token",
    "out",
    "watch",
    "help",
    "version",
];

// ---- command tree ------------------------------------------------------------

#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    op: Option<&'static OpSpec>,
}

fn build_tree() -> Node {
    let mut root = Node::default();
    for op in registry::all() {
        let mut cur = &mut root;
        for part in op.name.split('.') {
            cur = cur.children.entry(part.to_string()).or_default();
        }
        cur.op = Some(op);
    }
    root
}

fn flag_name(n: &str) -> String {
    n.replace('_', "-")
}

fn op_command(name: &str, op: &'static OpSpec) -> Command {
    let mut cmd = Command::new(name.to_string())
        .about(op.about)
        .allow_negative_numbers(true);
    if !op.examples.is_empty() {
        let ex = op
            .examples
            .iter()
            .map(|e| format!("  {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        cmd = cmd.after_help(format!("Examples:\n{ex}"));
    }
    for p in &op.params {
        let mut help = p.help.to_string();
        if p.required {
            help.push_str(" [required]");
        }
        if !p.choices.is_empty() {
            help.push_str(&format!(" [{}]", p.choices.join("|")));
        }
        let mut a = Arg::new(p.name).help(help);
        a = if p.positional {
            a.value_name(p.name.to_ascii_uppercase())
        } else {
            a.long(flag_name(p.name))
        };
        a = match (p.kind, p.multi) {
            (Kind::Bool, _) => a.action(ArgAction::SetTrue),
            (_, true) => a
                .action(ArgAction::Append)
                .num_args(1)
                .value_delimiter(None),
            (_, false) => a.action(ArgAction::Set).num_args(1),
        };
        if matches!(p.kind, Kind::Amount | Kind::Int) {
            a = a.allow_hyphen_values(true);
        }
        if let Some(d) = p.default {
            a = a.value_name(d.to_string()).hide_default_value(true);
        }
        cmd = cmd.arg(a);
    }
    cmd
}

fn node_command(name: &str, path: &str, node: &Node) -> Command {
    if let Some(op) = node.op {
        if node.children.is_empty() {
            return op_command(name, op);
        }
    }
    let about = GROUPS
        .iter()
        .find(|(g, _)| *g == path)
        .map(|(_, d)| *d)
        .unwrap_or("");
    let mut cmd = Command::new(name.to_string())
        .about(about)
        .subcommand_required(true)
        .arg_required_else_help(true);
    for (child, n) in &node.children {
        let cpath = if path.is_empty() {
            child.clone()
        } else {
            format!("{path}.{child}")
        };
        cmd = cmd.subcommand(node_command(child, &cpath, n));
    }
    cmd
}

pub fn build_cli() -> Command {
    let tree = build_tree();
    let mut cmd = Command::new("pext")
        .version(pext_engine::ops::system::VERSION)
        .about("pext: a personal finance engine for people and agents")
        .long_about(
            "pext keeps your money in a local SQLite ledger and exposes every capability as a command.\n\
             Output is a table on a terminal and JSON when piped. Run `pext commands` for a machine-readable\n\
             list of every command, or `pext schema <command>` for its JSON Schema.",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .arg(Arg::new("db").long("db").global(true).help("database file (env PEXT_DB, default ~/.pext/pext.db)"))
        .arg(
            Arg::new("output").long("output").short('o').global(true)
                .help("table | json | jsonl | csv | md | html | yaml (default: table on a terminal, json when piped; env PEXT_OUTPUT)"),
        )
        .arg(Arg::new("fields").long("fields").global(true).help("comma-separated columns to keep"))
        .arg(Arg::new("pretty").long("pretty").global(true).action(ArgAction::SetTrue).help("indent JSON output"))
        .arg(Arg::new("quiet").long("quiet").short('q').global(true).action(ArgAction::SetTrue).help("no warnings or notices on stderr"))
        .arg(Arg::new("no_color").long("no-color").global(true).action(ArgAction::SetTrue).help("disable colour"))
        .arg(Arg::new("dry_run").long("dry-run").global(true).action(ArgAction::SetTrue).help("run everything, report what would change, commit nothing"))
        .arg(Arg::new("yes").long("yes").short('y').global(true).action(ArgAction::SetTrue).help("confirm destructive operations"))
        .arg(Arg::new("idempotency_key").long("idempotency-key").global(true).help("make a write safe to retry"))
        .arg(Arg::new("token").long("token").global(true).help("access token (env PEXT_TOKEN)"))
        .arg(Arg::new("out").long("out").global(true).help("write the output to this file"))
        .arg(Arg::new("watch").long("watch").global(true).help("re-run a read command every N seconds"));
    for (child, n) in &tree.children {
        cmd = cmd.subcommand(node_command(child, child, n));
    }
    cmd
}

// ---- argument extraction -----------------------------------------------------

fn extract(op: &'static OpSpec, m: &ArgMatches) -> Map<String, Value> {
    let mut out = Map::new();
    for p in &op.params {
        match p.kind {
            Kind::Bool => {
                if m.get_flag(p.name) {
                    out.insert(p.name.into(), json!(true));
                }
            }
            _ => {
                if let Some(vals) = m.get_many::<String>(p.name) {
                    let vals: Vec<Value> = vals.map(|s| Value::String(s.clone())).collect();
                    if p.multi {
                        out.insert(p.name.into(), Value::Array(vals));
                    } else if let Some(v) = vals.into_iter().next() {
                        out.insert(p.name.into(), v);
                    }
                }
            }
        }
    }
    out
}

fn leaf<'a>(m: &'a ArgMatches, path: &mut Vec<String>) -> &'a ArgMatches {
    let mut cur = m;
    while let Some((name, sub)) = cur.subcommand() {
        path.push(name.to_string());
        cur = sub;
    }
    cur
}

// ---- running ------------------------------------------------------------------

fn peek_output(argv: &[String], env: &Env) -> Format {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "-o" || a == "--output" {
            if let Some(v) = it.next() {
                if let Ok(f) = Format::parse(v) {
                    return f;
                }
            }
        } else if let Some(v) = a.strip_prefix("--output=") {
            if let Ok(f) = Format::parse(v) {
                return f;
            }
        }
    }
    default_format(env)
}

fn default_format(env: &Env) -> Format {
    if let Some(Ok(f)) = env.output.as_deref().map(Format::parse) {
        return f;
    }
    if env.is_tty {
        Format::Table
    } else {
        Format::Json
    }
}

fn db_path(env: &Env, flag: Option<&String>) -> PathBuf {
    if let Some(p) = flag.cloned().or_else(|| env.db.clone()) {
        return PathBuf::from(p);
    }
    match &env.home {
        Some(h) => h.join(".pext").join("pext.db"),
        None => PathBuf::from("pext.db"),
    }
}

fn emit_error(
    e: &Error,
    format: Format,
    pretty: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let text = pext_render::render_error(format, e, pretty);
    let machine = matches!(
        format,
        Format::Json | Format::Jsonl | Format::Yaml | Format::Csv
    );
    let _ = if machine {
        writeln!(out, "{text}")
    } else {
        writeln!(err, "{text}")
    };
    e.kind.exit_code()
}

/// Run the CLI. Returns the process exit code.
pub fn run<I>(argv: I, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = String>,
{
    let argv: Vec<String> = argv.into_iter().collect();
    let peeked = peek_output(&argv, env);
    let matches = match build_cli().try_get_matches_from(&argv) {
        Ok(m) => m,
        Err(e) => {
            use clap::error::ErrorKind as CK;
            if matches!(
                e.kind(),
                CK::DisplayHelp | CK::DisplayVersion | CK::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let text = e.render().to_string();
                let machine_help = e.kind() == CK::DisplayHelpOnMissingArgumentOrSubcommand;
                if machine_help {
                    let _ = write!(err, "{text}");
                } else {
                    let _ = write!(out, "{text}");
                }
                return if machine_help {
                    ErrorKind::Usage.exit_code()
                } else {
                    0
                };
            }
            let msg = e.render().to_string();
            let first = msg
                .lines()
                .next()
                .unwrap_or("invalid usage")
                .trim_start_matches("error: ")
                .to_string();
            let hint = msg
                .lines()
                .find(|l| {
                    l.trim_start().starts_with("tip:") || l.trim_start().starts_with("Usage:")
                })
                .map(|l| l.trim().to_string());
            let mut er = Error::usage("invalid_usage", first);
            if let Some(h) = hint {
                er = er.with_hint(h);
            }
            return emit_error(&er, peeked, argv.iter().any(|a| a == "--pretty"), out, err);
        }
    };
    let pretty = matches.get_flag("pretty");
    let format = match matches.get_one::<String>("output") {
        Some(s) => match Format::parse(s) {
            Ok(f) => f,
            Err(e) => return emit_error(&e, default_format(env), pretty, out, err),
        },
        None => default_format(env),
    };
    match execute(&matches, env, format, pretty, out, err) {
        Ok(code) => code,
        Err(e) => emit_error(&e, format, pretty, out, err),
    }
}

fn execute(
    m: &ArgMatches,
    env: &Env,
    format: Format,
    pretty: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32> {
    let mut path = vec![];
    let sub = leaf(m, &mut path);
    let name = path.join(".");
    let op = registry::find(&name).ok_or_else(|| {
        Error::usage(
            "unknown_command",
            format!("unknown command '{}'", path.join(" ")),
        )
    })?;
    let raw = extract(op, sub);
    let quiet = m.get_flag("quiet");
    let today = match &env.today {
        Some(t) => Some(parse_today(t)?),
        None => None,
    };
    let cfg = EngineConfig {
        db_path: db_path(env, m.get_one::<String>("db")),
        today,
        plugin_dir: None,
    };
    let engine = Engine::new(cfg);
    let token = m
        .get_one::<String>("token")
        .cloned()
        .or_else(|| env.token.clone());
    let actor = resolve_actor(&engine, token.as_deref())?;
    let opts = CallOpts {
        dry_run: m.get_flag("dry_run"),
        yes: m.get_flag("yes"),
        idempotency_key: m.get_one::<String>("idempotency_key").cloned(),
    };
    let fields = m.get_one::<String>("fields").map(|f| {
        f.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });
    let color = env.is_tty
        && !env.no_color_env
        && !m.get_flag("no_color")
        && m.get_one::<String>("out").is_none();
    let watch = match m.get_one::<String>("watch") {
        Some(w) => {
            if op.mutating {
                return Err(Error::usage(
                    "watch_not_applicable",
                    "--watch works with read-only commands only",
                ));
            }
            Some(
                w.parse::<u64>()
                    .ok()
                    .filter(|n| (1..=3600).contains(n))
                    .ok_or_else(|| {
                        Error::usage(
                            "invalid_watch",
                            "--watch takes a number of seconds between 1 and 3600",
                        )
                        .with_field("watch")
                    })?,
            )
        }
        None => None,
    };

    loop {
        let output = engine.call(&op.name_owned(), raw.clone(), &opts, &actor)?;
        let base = default_currency(&engine, &actor);
        let ropts = Opts {
            fields: fields.clone(),
            pretty,
            color,
            default_currency: base,
        };
        let mut text = pext_render::render(format, &output.data, &output.meta, &ropts)?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(file) = m.get_one::<String>("out") {
            write_file(file, &text)?;
            if !quiet {
                let _ = writeln!(err, "wrote {} bytes to {file}", text.len());
            }
        } else {
            if watch.is_some() && env.is_tty {
                let _ = write!(out, "\x1b[2J\x1b[H");
            }
            let _ = write!(out, "{text}");
        }
        if !quiet && !matches!(format, Format::Json | Format::Yaml) {
            if let Some(Value::Array(ws)) = output.meta.get("warnings") {
                for w in ws {
                    let _ = writeln!(err, "warning: {}", w["message"].as_str().unwrap_or(""));
                }
            }
            if output.meta.get("dry_run") == Some(&json!(true)) {
                let n = output
                    .meta
                    .get("changes")
                    .and_then(|c| c.as_array())
                    .map_or(0, |c| c.len());
                let _ = writeln!(
                    err,
                    "dry run: {n} change(s) would be made; nothing was saved"
                );
            }
        }
        let code = output
            .meta
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .unwrap_or(0) as i32;
        match watch {
            Some(secs) => {
                let _ = out.flush();
                std::thread::sleep(std::time::Duration::from_secs(secs));
            }
            None => return Ok(code),
        }
    }
}

fn parse_today(s: &str) -> Result<NaiveDate> {
    pext_core::dates::parse_date(s).map_err(|e| e.with_hint("PEXT_TODAY must be YYYY-MM-DD"))
}

fn write_file(path: &str, text: &str) -> Result<()> {
    let target = PathBuf::from(path);
    let tmp = target.with_extension("pext-tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| Error::io("write_failed", format!("cannot write {path}: {e}")))?;
    std::fs::rename(&tmp, &target)
        .map_err(|e| Error::io("write_failed", format!("cannot write {path}: {e}")))
}

/// Resolve who is calling. Token support is wired up with the access-control
/// phase; until then a token can only be rejected clearly.
fn resolve_actor(_engine: &Engine, token: Option<&str>) -> Result<Actor> {
    match token {
        None => Ok(Actor::owner()),
        Some(_) => Err(Error::auth(
            "tokens_unavailable",
            "access tokens are not available in this build",
        )),
    }
}

fn default_currency(engine: &Engine, actor: &Actor) -> Option<String> {
    if !engine.cfg.db_path.exists() {
        return None;
    }
    let o = engine
        .call(
            "config.get",
            json_args(&[("key", "base_currency")]),
            &CallOpts::default(),
            actor,
        )
        .ok()?;
    o.data
        .get("value")
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn json_args(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect()
}

/// Names that registry parameters must not use (they would shadow global flags).
pub fn reserved_flag_names() -> &'static [&'static str] {
    GLOBALS
}

trait NameOwned {
    fn name_owned(&self) -> String;
}
impl NameOwned for OpSpec {
    fn name_owned(&self) -> String {
        self.name.to_string()
    }
}
