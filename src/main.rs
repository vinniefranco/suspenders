use clap::{Parser, Subcommand};

/// A terminal coding agent for small local models.
///
/// The top-level run args (root, headless, resume, ...) drive the default path -
/// a bare `suspenders`, a headless run, or `--write-config` - exactly as before.
/// An optional subcommand ([`Command`]) layers management commands (today the
/// `mcp` tree) on top WITHOUT changing that path: when no subcommand is given,
/// `command` is `None` and the run args take over. Clap allows the run args to
/// sit alongside an optional subcommand because the subcommand is the last
/// positional, so `suspenders <prompt>` parses `<prompt>` into `prompts` -
/// EXCEPT when the prompt's first token is a reserved subcommand name (today
/// just `mcp`): being the last positional, that token is captured as the
/// subcommand instead. A headless prompt that must begin with such a word can be
/// forced past the subcommand with a `--` separator.
#[derive(Parser, Debug)]
#[command(name = "suspenders")]
struct Cli {
    /// Project Root (defaults to the current directory)
    #[arg(long)]
    root: Option<std::path::PathBuf>,
    /// Run without the TUI, streaming events to stdout
    #[arg(long)]
    headless: bool,
    /// Resume from a Session Log path, "latest", or bare --resume to pick from a list
    #[arg(long, num_args = 0..=1, default_missing_value = "pick")]
    resume: Option<String>,
    /// Write a default config template to PATH (bare flag uses the XDG default), then exit
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    write_config: Option<String>,
    /// Overwrite an existing file when writing the config template
    #[arg(long)]
    force: bool,
    /// Prompt(s) to submit (headless runs them as sequential Runs)
    prompts: Vec<String>,
    /// A management subcommand; absent runs the default (TUI/headless) path.
    #[command(subcommand)]
    command: Option<Command>,
}

/// The management subcommands that layer over the default run path. Each is a
/// terminal action (do the thing, print, exit) resolved before any Session is
/// built, like `--write-config`.
#[derive(Subcommand, Debug)]
enum Command {
    /// Manage MCP servers in the config (add, remove, list).
    Mcp {
        #[command(subcommand)]
        command: suspenders::mcp::cli::McpCommand,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // A management subcommand is a terminal action, dispatched before any Session
    // is built (like --write-config): the mcp tree resolves the scope path, calls
    // the config seam, prints, and returns. The run args are ignored on this path.
    if let Some(Command::Mcp { command }) = cli.command {
        return suspenders::mcp::cli::dispatch(command);
    }

    // --write-config removes the hand-authoring friction (ADR-0031): resolve
    // the target (empty = XDG default, a rule the config seam owns), write the
    // base()-defaults template, and exit before any Session is built - works
    // for both TUI and headless.
    if let Some(path) = cli.write_config {
        let path = suspenders::session::SessionConfig::resolve_template_path(&path);
        suspenders::session::SessionConfig::write_template(&path, cli.force)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("wrote config template to {path}");
        return Ok(());
    }

    if cli.headless {
        suspenders::app::run_headless(cli.root, cli.resume, cli.prompts).await
    } else {
        suspenders::app::run_tui(cli.root, cli.resume).await
    }
}
