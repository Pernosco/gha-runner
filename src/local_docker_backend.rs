use std::io;

use futures::future::*;
use http_body_util::BodyExt;
use log::info;
use octocrab::{params, Octocrab};
use tokio::io::AsyncReadExt;

use super::*;

/// A wrapper around Docker container IDs to destroy containers when dropped,
/// to avoid leaking containers
struct DockerContainerInstance {
    id: String,
    keep: bool,
}

impl Drop for DockerContainerInstance {
    fn drop(&mut self) {
        if !self.keep {
            // kill container synchronously
            let mut cmd = std::process::Command::new("docker");
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
            cmd.args(&["rm", "-f", &self.id]);
            info!("Running {:?}", cmd);
            let _ = cmd.status();
        }
    }
}

impl DockerContainerInstance {
    fn new(id: String) -> DockerContainerInstance {
        DockerContainerInstance {
            id: id,
            keep: false,
        }
    }
    fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }
    fn id(&self) -> &str {
        self.id.as_str()
    }
}

/// A callback that receives stdout or stderr data from a step.
pub type OutputHandler = Box<dyn FnMut(&[u8])>;

/// Runs a GHA workflow using local Docker containers and the
/// docker CLI.
pub struct LocalDockerBackend {
    /// The containers we have spawned.
    container_instances: Vec<DockerContainerInstance>,
    stdout_handler: OutputHandler,
    stderr_handler: OutputHandler,
}

impl RunnerBackend for LocalDockerBackend {
    fn run<'a, F: FnMut(&[u8])>(
        &'a mut self,
        container: ContainerId,
        command: &'a str,
        stdout_filter: &'a mut F,
    ) -> Pin<Box<dyn Future<Output = i32> + 'a>> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new("docker");
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            cmd.args(&[
                "exec",
                "--tty",
                self.container_instances[container.0].id(),
                command,
            ]);
            info!("Running {:?}", cmd);
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => panic!("Failed to docker exec: {}", e),
            };
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let mut stdout_open = true;
            let mut stderr_open = true;
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            loop {
                tokio::select! {
                    r = stdout.read_buf(&mut stdout_buf), if stdout_open => {
                        if r.expect("reading stdout") > 0 {
                            stdout_filter(&stdout_buf);
                            (self.stdout_handler)(&stdout_buf);
                            stdout_buf.clear();
                        } else {
                            stdout_open = false;
                        }
                    },
                    r = stderr.read_buf(&mut stderr_buf), if stderr_open => {
                        if r.expect("reading stderr") > 0 {
                            (self.stderr_handler)(&stderr_buf);
                            stderr_buf.clear();
                        } else {
                            stderr_open = false;
                        }
                    },
                    else => break,
                }
            }
            child
                .wait()
                .await
                .expect("Should have waited successfully")
                .code()
                .unwrap_or(-1)
        })
    }
}

impl LocalDockerBackend {
    /// Create a new backend for running GHA with local docker containers
    pub fn new(
        job_runner: &JobRunner,
        github_dir: &Path,
        stdout_handler: OutputHandler,
        stderr_handler: OutputHandler,
    ) -> io::Result<LocalDockerBackend> {
        let bind_arg = format!(
            "type=bind,src={},dst=/github",
            github_dir.to_str().expect("Tempdir has non-UTF8 path?")
        );
        // Do this synchronously. Running 'docker run' commands async
        // risks leaking a container if the 'docker run' future is dropped before the result is
        // wrapped in DockerContainerInstance.
        let mut container_instances = Vec::new();
        let keep = std::env::var_os("KEEP_CONTAINERS").is_some();
        for image in job_runner.container_images() {
            let mut cmd = std::process::Command::new("docker");
            cmd.stdin(Stdio::null());
            cmd.args(&[
                "run",
                "--detach=true",
                "--tmpfs",
                "/tmp:exec",
                "--init",
                "--cap-add",
                "LINUX_IMMUTABLE",
                "--mount",
                &bind_arg,
                // Make the container sleep so we can spawn jobs into it
                "--entrypoint",
                "sleep",
                &image.0,
                "1000000",
            ]);
            info!("Running {:?}", cmd);
            let output = cmd.output()?;
            if !output.status.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Failed to create docker container for {}:\n{}",
                        image.0,
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
            let mut instance = DockerContainerInstance::new(
                std::str::from_utf8(&output.stdout)
                    .unwrap()
                    .trim()
                    .to_string(),
            );
            instance.set_keep(keep);
            container_instances.push(instance);
        }
        Ok(LocalDockerBackend {
            container_instances: container_instances,
            stdout_handler: stdout_handler,
            stderr_handler: stderr_handler,
        })
    }

    fn run_container_setup_command(&self, command: Vec<String>) -> io::Result<()> {
        for instance in self.container_instances.iter() {
            let mut cmd = std::process::Command::new("docker");
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
            cmd.args(&["exec", "--tty", instance.id()]);
            cmd.args(command.iter());
            info!("Running {:?}", cmd);
            if !cmd.status()?.success() {
                return Err(io::Error::new(io::ErrorKind::Other, "Command failed"));
            }
        }
        Ok(())
    }
}

/// Returns a Github personal access token that has no rights to access anything.
/// It will work for read-only access to public resources. For anything more you
/// have to supply your own token.
pub fn zero_access_token() -> &'static str {
    // Try to avoid it being picked up by scanners.
    // This token is harmless. It has no rights to access anything and is owned
    // by the pernosco-unauthorized Github account which itself has no special
    // access to anything; it exists solely to own this token.
    concat!("ghp_", "7EMsGDj8ZsOEJxDSXBoC9XsEjFgMWw2NvXQk")
}

/// Either fetches workflow data from the repository (if 'workflow' is a relative path)
/// or reads it from a file on the local filesystem with that name.
/// Panics if something goes wrong.
pub async fn get_workflow(
    github: &Octocrab,
    owner: &str,
    repo: &str,
    sha: &str,
    workflow: &str,
) -> (Vec<u8>, String) {
    if workflow.starts_with('/') {
        let data = fs::read(&workflow).unwrap();
        (data, format!(".github/workflows/{}", workflow))
    } else {
        let commit: params::repos::Commitish = sha.to_string().into();
        let response = github
            .repos(owner, repo)
            .raw_file(commit, workflow)
            .await
            .unwrap()
            .into_body();
        let bytes = response.collect().await.unwrap().to_bytes();
        info!("Fetched {}, {} bytes", workflow, bytes.len());
        (bytes.to_vec(), workflow.to_string())
    }
}

/// The result of `run_workflow_with_local_backend`
#[derive(Clone, Eq, PartialEq)]
pub enum WorkflowResult {
    /// All steps completed successfully
    AllStepsPassed,
    /// A step failed.
    StepFailed {
        /// The name of the failing step (if it has one)
        step_name: Option<String>,
        /// The step's exit code
        exit_code: i32,
    },
}

/// Optional parameters for `run_workflow_with_local_backend`
pub struct LocalDockerOptions {
    /// The Github personal access token to pass to actions (e.g. checkout);
    /// this must be valid!
    pub access_token: String,
    /// This gets invoked for each write to stdout by an action step.
    pub stdout_handler: Box<dyn FnMut(&[u8])>,
    /// This gets invoked for each write to stderr by an action step.
    pub stderr_handler: Box<dyn FnMut(&[u8])>,
    /// Commands to execute in each container after it has been created
    pub container_setup_commands: Vec<Vec<String>>,
    /// This gets applied to each step command before running it
    pub modify_step_command: Box<dyn FnMut(&mut Vec<String>)>,
    /// This runs before the temp directory is removed; takes
    /// the name of the temp directory (mapped to /github in containers).
    pub before_temp_dir_removal_hook: Box<dyn FnOnce(&Path)>,
}

impl Default for LocalDockerOptions {
    fn default() -> LocalDockerOptions {
        LocalDockerOptions {
            access_token: zero_access_token().to_string(),
            stdout_handler: Box::new(|_| ()),
            stderr_handler: Box::new(|_| ()),
            container_setup_commands: Vec::new(),
            modify_step_command: Box::new(|_| ()),
            before_temp_dir_removal_hook: Box::new(|_| ()),
        }
    }
}

/// Run a complete workflow using the local docker backend and DefaultImageMapping.
/// If `workflow` is an absolute path we read that file to get the workflow data,
/// otherwise it's a repo-relative path and we fetch the workflow data from the repo.
/// Panics if something goes wrong.
pub async fn run_workflow_with_local_backend(
    owner: &str,
    repo: &str,
    sha: &str,
    workflow: &str,
    job_name: &str,
    images: &DockerImageMapping,
    mut options: LocalDockerOptions,
) -> WorkflowResult {
    let github = Octocrab::default();
    let (workflow_data, workflow_repo_path) =
        get_workflow(&github, owner, repo, sha, workflow).await;

    let temp_dir = tempfile::tempdir().expect("Can't create tempdir");

    let ctx = RunnerContext {
        github: github,
        owner: owner.to_string(),
        repo: repo.to_string(),
        commit_sha: hex::decode(sha.as_bytes()).unwrap(),
        // XXX fill this in with a command line option
        commit_ref: None,
        global_dir_host: temp_dir.path().to_path_buf(),
        workflow_repo_path: workflow_repo_path,
        run_id: 1u64.into(),
        run_number: 1,
        job_id: 1u64.into(),
        actor: "gha-runner".to_string(),
        token: options.access_token,
        override_env: Vec::new(),
    };

    let runner = Runner::new(ctx, &workflow_data).await.unwrap();

    let job_description = if let Some(jd) = runner
        .job_descriptions()
        .iter()
        .find(|jd| jd.name() == job_name)
    {
        jd
    } else {
        panic!("Can't find job '{}'", job_name);
    };
    let mut job_runner = runner
        .job_runner(job_description, images)
        .await
        .expect("Failed to create JobRunner");
    let mut backend = LocalDockerBackend::new(
        &job_runner,
        temp_dir.path(),
        options.stdout_handler,
        options.stderr_handler,
    )
    .expect("Failed to create docker backend");
    for cmd in options.container_setup_commands {
        backend.run_container_setup_command(cmd).unwrap();
    }

    loop {
        let step = job_runner.next_step_index();
        if step >= job_runner.step_count() {
            break;
        }
        let step_name = job_runner
            .next_step_name()
            .expect("Failed to get step name");
        info!(
            "Running step {} {}",
            step.0,
            step_name.as_deref().unwrap_or("<anonymous>")
        );
        let exit_code = job_runner
            .run_next_step(&mut options.modify_step_command, &mut backend)
            .await
            .expect("Failed to run step");
        if exit_code != 0 {
            (options.before_temp_dir_removal_hook)(temp_dir.path());
            return WorkflowResult::StepFailed {
                step_name: step_name,
                exit_code: exit_code,
            };
        }
    }
    (options.before_temp_dir_removal_hook)(temp_dir.path());
    WorkflowResult::AllStepsPassed
}
