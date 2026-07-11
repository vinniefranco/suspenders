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
    /// Resume from a Session Log path, or "latest"
    #[arg(long)]
    resume: Option<String>,
    /// Prompt(s) to submit (headless runs them as sequential Turns)
    prompts: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.headless {
        suspenders::app::run_headless(cli.root, cli.resume, cli.prompts).await
    } else {
        suspenders::app::run_tui(cli.root, cli.resume).await
    }
}
