use clap::Parser;

/// A terminal coding agent for small local models.
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
    /// Prompt(s) to submit (headless runs them as sequential Turns)
    prompts: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --write-config removes the hand-authoring friction (ADR-0031): resolve the
    // path (empty = XDG default), write the base()-defaults template, and exit
    // before any Session is built — works for both TUI and headless.
    if let Some(path) = cli.write_config {
        let path = if path.is_empty() {
            suspenders::session::default_config_path()
        } else {
            path
        };
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
