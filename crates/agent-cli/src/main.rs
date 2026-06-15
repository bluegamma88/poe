use std::{env, error::Error, path::PathBuf, process};

use agent_config::{Config, sessions_dir_from_home_dir};
use agent_core::OpenRouterClient;
use agent_exec::{ExecOptions, run_with_model_live};
use agent_tui::TuiOptions;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "poe",
    version,
    about = "A small coding agent harness. Named after Poe, the AI from Altered Carbon."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(hide = true)]
    bare_prompt: Vec<String>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum Command {
    /// Run one non-interactive agent turn.
    Exec {
        /// Print one JSON event per stdout line.
        #[arg(long)]
        json: bool,
        /// Prompt to send to the agent.
        #[arg(required = true, trailing_var_arg = true)]
        prompt: Vec<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Tui,
    Exec { json: bool, prompt: Vec<String> },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match mode_from_cli(cli)? {
        Mode::Tui => run_tui().await,
        Mode::Exec { json, prompt } => run_exec(json, prompt).await,
    }
}

fn mode_from_cli(cli: Cli) -> Result<Mode, Box<dyn Error>> {
    match cli.command {
        Some(Command::Exec { json, prompt }) => Ok(Mode::Exec { json, prompt }),
        None if cli.bare_prompt.is_empty() => Ok(Mode::Tui),
        None => Err(
            "unexpected prompt without a subcommand; use `poe exec PROMPT` or run `poe` for interactive mode"
                .into(),
        ),
    }
}

async fn run_tui() -> Result<(), Box<dyn Error>> {
    let model_config = Config::load()?.resolve_model_config()?;
    let model = OpenRouterClient::new(model_config.model, model_config.api_key);

    let sessions_dir = env::var_os("HOME").map(sessions_dir_from_home_dir);

    agent_tui::run_with_model(
        TuiOptions {
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            sessions_dir,
        },
        model,
    )
    .await?;

    Ok(())
}

async fn run_exec(json: bool, prompt_parts: Vec<String>) -> Result<(), Box<dyn Error>> {
    let model_config = Config::load()?.resolve_model_config()?;
    let model = OpenRouterClient::new(model_config.model, model_config.api_key);

    let sessions_dir = env::var_os("HOME").map(sessions_dir_from_home_dir);

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let exit_code = run_with_model_live(
        ExecOptions {
            prompt: prompt_parts.join(" "),
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            json,
            sessions_dir,
        },
        model,
        &mut stdout,
        &mut stderr,
    )
    .await?;

    if exit_code == 0 {
        Ok(())
    } else {
        process::exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_selects_tui_mode() {
        let cli = Cli::try_parse_from(["poe"]).expect("parse cli");

        assert_eq!(mode_from_cli(cli).expect("resolve mode"), Mode::Tui);
    }

    #[test]
    fn exec_json_selects_exec_mode() {
        let cli =
            Cli::try_parse_from(["poe", "exec", "--json", "say", "hello"]).expect("parse cli");

        assert_eq!(
            mode_from_cli(cli).expect("resolve mode"),
            Mode::Exec {
                json: true,
                prompt: vec!["say".to_string(), "hello".to_string()],
            }
        );
    }

    #[test]
    fn bare_prompt_is_rejected() {
        let cli = Cli::try_parse_from(["poe", "say hello"]).expect("parse cli");
        let error = mode_from_cli(cli).expect_err("reject bare prompt");

        assert!(error.to_string().contains("unexpected prompt"));
    }
}
