use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;
use tokio::runtime;

use gha_runner::*;

#[derive(Parser)]
#[command(author, version, about)]
/// Run a Gitub Actions workflow locally
struct Cli {
    owner: String,
    repo: String,
    sha: String,
    /// Can be either an absolute path to a file containing the workflow or a path relative to the repo
    workflow: String,
    job_name: String,
    #[arg(
        long = "image-path",
        default_value = "ghcr.io/catthehacker/ubuntu:full"
    )]
    /// Path to image names
    image_path: String,
    #[arg(long = "strace")]
    /// Run everything under strace and emit the results for the last executed step to the given file
    strace: Option<PathBuf>,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async move {
        let mut options = LocalDockerOptions {
            stdout_handler: Box::new(|bytes| io::stdout().lock().write_all(bytes).unwrap()),
            stderr_handler: Box::new(|bytes| io::stderr().lock().write_all(bytes).unwrap()),
            ..Default::default()
        };
        if let Some(output_path) = cli.strace {
            options.container_setup_commands.push(
                vec!["sudo", "apt", "update"]
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect(),
            );
            options.container_setup_commands.push(
                vec!["sudo", "apt", "install", "-y", "strace"]
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect(),
            );
            options.modify_step_command = Box::new(|cmd: &mut Vec<String>| {
                let mut new_cmd = vec!["strace", "-f", "-o", "/github/.gha-runner/strace.out"]
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                new_cmd.append(cmd);
                *cmd = new_cmd;
            });
            options.before_temp_dir_removal_hook = Box::new(|path: &Path| {
                fs::copy(path.join(".gha-runner/strace.out"), output_path).unwrap();
            });
        }
        let images = DockerImageMapping {
            ubuntu_18_04: format!("{}-18.04", &cli.image_path).into(),
            ubuntu_20_04: format!("{}-20.04", &cli.image_path).into(),
        };
        match run_workflow_with_local_backend(
            &cli.owner,
            &cli.repo,
            &cli.sha,
            &cli.workflow,
            &cli.job_name,
            &images,
            options,
        )
        .await
        {
            WorkflowResult::AllStepsPassed => (),
            WorkflowResult::StepFailed {
                step_name,
                exit_code,
            } => {
                eprintln!(
                    "Step '{}' exited with code {}, aborting",
                    step_name.as_deref().unwrap_or("<anonymous>"),
                    exit_code
                );
                process::exit(exit_code);
            }
        }
    });
}
