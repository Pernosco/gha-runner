#![deny(missing_docs)]

//! `gha-runner` runs Github Actions workflows. It supports pluggable backends
//! via the `RunnerBackend` trait, and provides a `LocalDockerBackend` implementation
//! that runs workflows using local docker containers. You can also analyze the
//! structure of workflow jobs and modify step execution.
//!
//! # Example
//! ```
//! use gha_runner::*;
//! let images: DockerImageMapping = DockerImageMapping {
//!     ubuntu_18_04: "ghcr.io/catthehacker/ubuntu:act-18.04".into(),
//!     ubuntu_20_04: "ghcr.io/catthehacker/ubuntu:act-20.04".into(),
//! };
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .enable_all()
//!     .build()
//!     .unwrap();
//! runtime.block_on(async move {
//!     run_workflow_with_local_backend(
//!         "Pernosco",
//!         "github-actions-test",
//!         "6475d0f048a72996e3bd559cdd3763f53fe3d072",
//!         ".github/workflows/build.yml",
//!         "Build+test (stable, ubuntu-18.04)",
//!         &images,
//!         LocalDockerOptions::default(),
//!     ).await;
//! });
//! ```
//!
//! # Lower-level API
//!
//! * Fill out a `RunnerContext`
//! * Call `Runner::new()` to create a `Runner`
//! * Call `Runner::job_descriptions()` to get a list of `JobDescriptions` and pick a job
//! * Call `Runner::job_runner()` to create a `JobRunner`
//! * Call `JobRunner::container_images()` to get a list of Docker container images that will be needed, and create one container per image
//! * Call `JobRunner::run_next_step()` repeatedly to run each job step, until `next_step_index() >= step_count()`

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::str;

use backtrace::Backtrace;
use linked_hash_map::LinkedHashMap;
use log::{debug, info, warn};
use octocrab::models::JobId;
use octocrab::models::RunId;
use octocrab::{params, Octocrab};
use serde_yaml::Value;

mod contexts;
mod expressions;
mod local_docker_backend;
mod models;

use contexts::*;
use expressions::ContextResolver;
pub use local_docker_backend::*;
use models::*;

/// This contains various configuration values needed to run a Github Actions workflow.
/// Most of these values are exposed to actions via [standard GHA environment variables](https://docs.github.com/en/actions/learn-github-actions/environment-variables).
#[derive(Clone)]
pub struct RunnerContext {
    /// The `Octocrab` Github client used to fetch workflow resources.
    /// This can be configured with or without authentication; in the latter case
    /// all required resources must be public.
    pub github: Octocrab,
    /// The Github owner name, e.g. `Pernosco`.
    pub owner: String,
    /// The Github repo name, e.g. `github-actions-test`.
    pub repo: String,
    /// The repo commit-SHA decoded from hex, e.g. using `hex::decode(sha_string.has_bytes())`.
    pub commit_sha: Vec<u8>,
    /// An optional git ref for the commit. Exposed as `$GITHUB_REF` in the GHA steps.
    pub commit_ref: Option<String>,
    /// A global working directory whose contents are exposed to all containers as `/github`.
    /// The runner will create files under this directory.
    pub global_dir_host: PathBuf,
    /// Name of workflow file in the repo, e.g. `.github/workflows/build.yml`.
    pub workflow_repo_path: String,
    /// The workflow run ID. Exposed as `$GITHUB_RUN_ID` in the GHA steps.
    /// Can usually just be (e.g.) `1`, but you might want to match the ID of some real
    /// workflow run.
    pub run_id: RunId,
    /// The rerun number. Exposed as `$GITHUB_RUN_NUMBER` in the GHA steps. Can usually
    /// just be `1`.
    pub run_number: i64,
    /// The job ID. Exposed as `$GITHUB_JOB` in the GHA steps. Can usually just be `1`.
    pub job_id: JobId,
    /// Exposed as `$GITHUB_ACTOR` in the GHA steps.
    pub actor: String,
    /// A Github [personal access token](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/creating-a-personal-access-token)
    /// for actions to use for checkouts and other Github operations. This *must* be valid.
    /// See `zero_access_token()` for a token you can use.
    pub token: String,
    /// (key, value) pairs that override environment variable settings in a step.
    /// These are applied after all workflow-defined environment variables.
    pub override_env: Vec<(String, String)>,
}

const GITHUB_WORKSPACE: &str = "/github/workspace";
const GITHUB_COM: &str = "https://github.com";
const API_GITHUB_COM: &str = "https://api.github.com";
const API_GITHUB_COM_GRAPHQL: &str = "https://api.github.com/graphql";

impl RunnerContext {
    // See https://docs.github.com/en/actions/reference/environment-variables
    fn create_env(&self, workflow_name: Option<String>) -> LinkedHashMap<String, String> {
        let mut ret = LinkedHashMap::new();
        let mut insert = |key: &str, value: String| {
            ret.insert(key.to_string(), value);
        };
        insert("CI", "true".to_string());
        insert(
            "GITHUB_WORKFLOW",
            workflow_name.unwrap_or_else(|| self.workflow_repo_path.clone()),
        );
        insert("GITHUB_RUN_ID", self.run_id.to_string());
        insert("GITHUB_RUN_NUMBER", self.run_number.to_string());
        insert("GITHUB_JOB", self.job_id.to_string());
        // XXX GITHUB_ACTION
        // XXX Support GITHUB_ACTION_PATH when we support composite actions
        insert("GITHUB_ACTIONS", "true".to_string());
        insert("GITHUB_ACTOR", self.actor.clone());
        insert("GITHUB_REPOSITORY", format!("{}/{}", self.owner, self.repo));
        // XXX we probably should support overriding this. If we do we would need
        // to support GITHUB_HEAD_REF/GITHUB_BASE_REF
        insert("GITHUB_EVENT_NAME", "push".to_string());
        insert("GITHUB_EVENT_PATH", "/github/.gha-runner/event".to_string());
        insert("GITHUB_PATH", "/github/.gha-runner/path".to_string());
        insert("GITHUB_ENV", "/github/.gha-runner/env".to_string());
        insert("GITHUB_WORKSPACE", GITHUB_WORKSPACE.to_string());
        insert("GITHUB_SHA", hex::encode(&self.commit_sha));
        if let Some(r) = self.commit_ref.as_ref() {
            insert("GITHUB_REF", r.to_string());
        }
        insert("GITHUB_SERVER_URL", GITHUB_COM.to_string());
        insert("GITHUB_API_URL", API_GITHUB_COM.to_string());
        insert("GITHUB_GRAPHQL_URL", API_GITHUB_COM_GRAPHQL.to_string());
        insert("GITHUB_TOKEN", self.token.clone());
        insert("RUNNER_OS", "Linux".to_string());
        insert("RUNNER_TEMP", "/tmp".to_string());
        insert("RUNNER_TOOL_CACHE", "/opt/hostedtoolcache".to_string());
        ret
    }
    fn apply_env(
        &self,
        mut target: LinkedHashMap<String, String>,
    ) -> LinkedHashMap<String, String> {
        for v in self.override_env.iter() {
            target.insert(v.0.to_string(), v.1.to_string());
        }
        target
    }
}

/// A description of a job in a workflow.
pub struct JobDescription {
    name: String,
    matrix_values: LinkedHashMap<String, Value>,
    job_env: LinkedHashMap<String, String>,
    runs_on: Vec<String>,
    steps: Vec<models::Step>,
}

/// Assigns full docker image names to GHA 'runs-on' names
pub struct DockerImageMapping {
    /// Full docker image name for ubuntu-18.04
    pub ubuntu_18_04: ContainerImage,
    /// Full docker image name for ubuntu-20.04
    pub ubuntu_20_04: ContainerImage,
}

impl JobDescription {
    /// The name of the job. This includes values added due to `matrix`.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The runs-on OS name(s).
    pub fn runs_on(&self) -> &[String] {
        &self.runs_on
    }
    /// Container image name to use for the main container.
    pub fn main_container(&self, image_mapping: &DockerImageMapping) -> Option<ContainerImage> {
        if self.runs_on.is_empty() {
            return None;
        }
        let img = match self.runs_on[0].as_str() {
            // XXX what to do about self-hosted runners?
            "ubuntu-latest" | "ubuntu-20.04" => &image_mapping.ubuntu_20_04,
            "ubuntu-18.04" => &image_mapping.ubuntu_18_04,
            _ => return None,
        };
        Some(img.clone())
    }
}

/// An index into the steps in the workflow YAML. This is not the same thing
/// as Github's "step number", which includes invisible steps like
/// job creation and isn't always numbered consecutively.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct StepIndex(pub u32);

/// Runs a specific Job.
/// The simplest way to use this is to repeatedly call `run_next_step()` until
/// `next_step_index() >= step_count()`.
pub struct JobRunner {
    ctx: RunnerContext,
    job_name: String,
    matrix_values: LinkedHashMap<String, Value>,
    /// Environment variables to set in future steps
    env: LinkedHashMap<String, String>,
    /// Path entries to add in future steps. The entries are added
    /// in reverse order, i.e. the last entry in `paths` is at the
    /// start of the PATH.
    paths: Vec<String>,
    /// The first image is the default image.
    container_images: Vec<ContainerImage>,
    /// The full list of steps
    steps: Vec<models::Step>,
    outputs: Vec<LinkedHashMap<String, String>>,
    step_index: StepIndex,
}

/// A full Docker container image name, e.g `ghcr.io/catthehacker/ubuntu:js-18.04`
#[derive(Clone, Eq, PartialEq)]
pub struct ContainerImage(pub String);

impl From<&str> for ContainerImage {
    fn from(s: &str) -> ContainerImage {
        ContainerImage(s.to_string())
    }
}

impl From<String> for ContainerImage {
    fn from(s: String) -> ContainerImage {
        ContainerImage(s)
    }
}

/// Index into `JobRunner::containers`
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ContainerId(pub usize);

/// A backend that can run tasks in containers.
pub trait RunnerBackend {
    /// Run a command in the context of the container, returning its exit code.
    /// There are no parameters, environment variables etc --- we emit script files
    /// into the shared filesystem that set those up.
    /// `container` is an index into the `container_images()` array.
    /// `command` is the path to the command *inside the container*, starting with
    /// "/github".
    /// `stdout_filter` is a callback that gets invoked for each chunk of data
    /// emitted to the command's stdout. (We use this to process
    /// "[workflow commands](https://docs.github.com/en/actions/learn-github-actions/workflow-commands-for-github-actions)".)
    fn run<'a, F: FnMut(&[u8])>(
        &'a mut self,
        container: ContainerId,
        command: &'a str,
        stdout_filter: &'a mut F,
    ) -> Pin<Box<dyn Future<Output = i32> + 'a>>;
}

async fn untar_response(
    action: &str,
    mut response: reqwest::Response,
    dir: &Path,
) -> Result<(), RunnerErrorKind> {
    let mut command = Command::new("tar");
    // Github's tarball contains a toplevel directory (e.g. 'actions-checkout-f1d3225')
    // so we strip that off.
    command
        .args(&["zxf", "-", "--strip-components=1"])
        .current_dir(dir)
        .stdin(Stdio::piped());
    let mut child = command.spawn().expect("Can't find 'tar'");
    while let Some(b) =
        response
            .chunk()
            .await
            .map_err(|e| RunnerErrorKind::ActionDownloadError {
                action: action.to_string(),
                inner: Box::new(e),
            })?
    {
        child.stdin.as_mut().unwrap().write_all(&b).map_err(|e| {
            RunnerErrorKind::ActionDownloadError {
                action: action.to_string(),
                inner: Box::new(e),
            }
        })?;
    }
    // Close stdin so tar terminates
    child.stdin = None;
    let status = child.wait().unwrap();
    if !status.success() {
        return Err(RunnerErrorKind::ActionDownloadError {
            action: action.to_string(),
            inner: format!("tar failed: {}", status).into(),
        });
    }
    Ok(())
}

fn envify(s: &str) -> String {
    let mut ret = String::new();
    for ch in s.chars() {
        let replace = match ch {
            'A'..='Z' | '0'..='9' => ch,
            'a'..='z' => ch.to_ascii_uppercase(),
            _ => '_',
        };
        ret.push(replace);
    }
    ret
}

fn shell_quote(s: &str) -> String {
    let mut ret = String::new();
    let mut quote = false;
    ret.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            quote = true;
            ret.push_str("'\"'\"'");
        } else {
            if !matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '=' | '.' | '/' | ':')
            {
                quote = true;
            }
            ret.push(ch);
        }
    }
    ret.push('\'');
    if quote {
        ret
    } else {
        s.to_string()
    }
}

struct RunSpec {
    env: LinkedHashMap<String, String>,
    /// The last entry should be prepended first
    paths: Vec<String>,
    working_directory: Option<String>,
    command: Vec<String>,
}

struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    fn new() -> LineBuffer {
        LineBuffer { buf: Vec::new() }
    }
    fn append(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }
    /// Runs closure for complete lines ending in \n, with the \n stripped.
    fn take_lines<F: FnMut(&[u8])>(&mut self, mut f: F) {
        let mut offset = 0;
        while let Some(next) = self.buf[offset..].iter().position(|c| *c == b'\n') {
            let next = offset + next;
            f(&self.buf[offset..next]);
            offset = next + 1;
        }
        self.buf.drain(..offset);
    }
}

fn parse_simple_shell_command(s: &str) -> Option<Vec<String>> {
    for ch in s.chars() {
        if !matches!(ch, '0'..='9' | 'a'..='z' | 'A'..='Z' | '-' | '/' | '.' | ' ' | '\n') {
            return None;
        }
    }
    Some(s.split_ascii_whitespace().map(|v| v.to_string()).collect())
}

/// Create a directory that's publicly writeable.
fn create_public_writable_dir(path: &Path) {
    let _ = fs::create_dir(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
}

/// Create a file that's publicly writeable.
fn create_public_writable_file(path: &Path) {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(path)
        .unwrap();
    // umask may have stopped us setting the permissions we wanted,
    // so reset permissions now.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
}

impl JobRunner {
    /// The job requires a specific set of containers, one container per
    /// entry in the returned list. Each container must use the given
    /// `ContainerImage`.
    pub fn container_images(&self) -> &[ContainerImage] {
        &self.container_images
    }
    /// The total number of steps in the job.
    pub fn step_count(&self) -> StepIndex {
        StepIndex(self.steps.len() as u32)
    }
    /// The index of the next step to be executed.
    pub fn next_step_index(&self) -> StepIndex {
        self.step_index
    }
    /// The name of the next step to be executed, if there is one.
    pub fn next_step_name(&self) -> Result<Option<String>, RunnerError> {
        let step_context = StepContext::new(
            &self.ctx,
            &self.matrix_values,
            &self.job_name,
            self.step_index,
        );
        self.steps[self.step_index.0 as usize]
            .clone()
            .take_name(&step_context)
    }
    /// The job name.
    pub fn job_name(&self) -> &str {
        &self.job_name
    }
    /// Look up a step by name. We don't have access to variables set by previous steps
    /// so this might not work in obscure cases...
    pub fn find_step_by_name(&self, step_name: &str) -> Result<Option<StepIndex>, RunnerError> {
        for (index, step) in self.steps[(self.step_index.0 as usize)..]
            .iter()
            .enumerate()
        {
            let step_index = StepIndex(index as u32);
            let step_context =
                PreStepContext::new(&self.ctx, &self.matrix_values, &self.job_name, step_index);
            let mut step = step.clone();
            let name = step.take_name_pre(&step_context)?;
            if name.as_deref() == Some(step_name) {
                return Ok(Some(step_index));
            }
        }
        Ok(None)
    }
    /// Get the environment variables set in a specific step. We don't have access to variables set by previous steps
    /// so this might not work in obscure cases...
    pub fn peek_step_env(
        &self,
        step_index: StepIndex,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        let mut step = self.steps[step_index.0 as usize].clone();
        let step_context =
            PreStepContext::new(&self.ctx, &self.matrix_values, &self.job_name, step_index);
        step.take_env_pre(&step_context)
            .map(|env| self.ctx.apply_env(env))
    }
    /// `interpose` lets you modify the command that will be run for the step.
    pub async fn run_next_step<B: RunnerBackend, I>(
        &mut self,
        interpose: I,
        backend: &mut B,
    ) -> Result<i32, RunnerError>
    where
        I: FnOnce(&mut Vec<String>),
    {
        if self.step_index.0 == 0 {
            let _ = create_public_writable_dir(&self.ctx.global_dir_host.join("workspace"));
            let _ = create_public_writable_dir(&self.ctx.global_dir_host.join(".gha-runner"));
            fs::write(self.ctx.global_dir_host.join(".gha-runner/event"), "{}").unwrap();
            let _ = create_public_writable_dir(
                &self.ctx.global_dir_host.join(".gha-runner/hostedtoolcache"),
            );
        }
        let _ = fs::create_dir(
            self.ctx
                .global_dir_host
                .join(format!(".gha-runner/step{}", self.step_index.0)),
        );

        let mut step = self.steps[self.step_index.0 as usize].clone();
        let spec = {
            let step_context = StepContext::new(
                &self.ctx,
                &self.matrix_values,
                &self.job_name,
                self.step_index,
            );
            let mut env = self.env.clone();
            for (k, v) in self.ctx.apply_env(step.take_env(&step_context)?) {
                env.insert(k, v);
            }
            let mut spec = RunSpec {
                env: env,
                paths: self.paths.clone(),
                working_directory: step.take_working_directory(&step_context)?,
                command: Vec::new(),
            };
            if let Some(uses) = step.take_uses(&step_context)? {
                self.configure_action(&mut step, uses, &mut spec).await?;
            } else if let Some(run) = step.take_run(&step_context)? {
                self.configure_command(&mut step, run, &mut spec).await?;
            } else {
                return Err(RunnerErrorKind::RequiredFieldMissing {
                    field_name: "uses/run",
                    got: step.0,
                }
                .error(&step_context));
            }
            spec
        };
        let ret = self.run_command("run", spec, interpose, backend).await;
        self.step_index.0 += 1;
        ret
    }

    async fn run_command<B: RunnerBackend, I>(
        &mut self,
        script_name: &str,
        mut spec: RunSpec,
        interpose: I,
        backend: &mut B,
    ) -> Result<i32, RunnerError>
    where
        I: FnOnce(&mut Vec<String>),
    {
        let script_path = format!(".gha-runner/step{}/{}", self.step_index.0, script_name);
        {
            // Ensure file is closed before we run it.
            let mut script_file = BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o777)
                    .open(self.ctx.global_dir_host.join(&script_path))
                    .unwrap(),
            );
            // Use a login shell since some path etc setup may only happen with a login shell
            writeln!(&mut script_file, "#!/bin/bash -l").unwrap();
            for (k, v) in spec.env {
                if k.contains('=') {
                    return Err(RunnerErrorKind::InvalidEnvironmentVariableName { name: k }.into());
                }
                writeln!(
                    &mut script_file,
                    "export {}={}",
                    shell_quote(&k),
                    shell_quote(&v)
                )
                .unwrap();
            }
            if !spec.paths.is_empty() {
                write!(&mut script_file, "export PATH=").unwrap();
                for p in spec.paths.iter().rev() {
                    write!(&mut script_file, "{}:", p).unwrap();
                }
                writeln!(&mut script_file, "$PATH").unwrap();
            }
            if self.step_index.0 == 0 {
                writeln!(
                    &mut script_file,
                    "ln -s /github/.gha-runner/hostedtoolcache $RUNNER_TOOL_CACHE"
                )
                .unwrap();
            }
            writeln!(&mut script_file, "cd /github/workspace").unwrap();
            if let Some(d) = spec.working_directory {
                writeln!(&mut script_file, "cd {}", shell_quote(&d)).unwrap();
            }
            write!(&mut script_file, "exec").unwrap();
            interpose(&mut spec.command);
            for arg in spec.command {
                write!(&mut script_file, " {}", shell_quote(&arg)).unwrap();
            }
        }

        let mut line_buffer = LineBuffer::new();
        let mut stop_token: Option<Vec<u8>> = None;
        let mut outputs = LinkedHashMap::new();
        let mut stdout_filter = |data: &[u8]| {
            line_buffer.append(data);
            line_buffer.take_lines(|line| {
                if line.len() < 2 || &line[0..2] != b"::" {
                    return;
                }
                if let Some(token) = stop_token.as_ref() {
                    if line[2..].ends_with(b"::") && &line[2..(line.len() - 2)] == token {
                        stop_token = None;
                    }
                    return;
                }
                if let Some(token) = line.strip_prefix(b"::stop-commands::") {
                    stop_token = Some(token.to_vec());
                    return;
                }
                if let Some(rest) = line.strip_prefix(b"::set-output name=") {
                    if let Ok(rest) = str::from_utf8(rest) {
                        if let Some(p) = rest.find("::") {
                            outputs.insert(rest[..p].to_string(), rest[(p + 2)..].to_string());
                        } else {
                            warn!(
                                "No '::' in set-output command: {}",
                                String::from_utf8_lossy(line)
                            );
                        }
                    } else {
                        warn!(
                            "Non-UTF8 set-output command: {}",
                            String::from_utf8_lossy(line)
                        );
                    }
                    return;
                }
                if line == b"::save-state" {
                    warn!("Ignoring ::save-state for now");
                }
            })
        };
        create_public_writable_file(&self.github_path_path());
        create_public_writable_file(&self.github_env_path());
        let ret = backend
            .run(
                ContainerId(0),
                &format!("/github/{}", script_path),
                &mut stdout_filter,
            )
            .await;
        self.outputs.push(outputs);
        self.update_env_from_file();
        self.update_path_from_file();
        Ok(ret)
    }

    fn github_path_path(&self) -> PathBuf {
        self.ctx.global_dir_host.join(".gha-runner/path")
    }
    fn github_env_path(&self) -> PathBuf {
        self.ctx.global_dir_host.join(".gha-runner/env")
    }

    fn update_env_from_file(&mut self) {
        let mut env_file = BufReader::new(File::open(self.github_env_path()).unwrap());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let len = env_file.read_until(b'\n', &mut buf).unwrap();
            if len == 0 {
                break;
            }
            debug!(
                "env line for step {}: {}",
                self.step_index.0,
                String::from_utf8_lossy(&buf)
            );
            if buf.last() == Some(&b'\n') {
                buf.truncate(buf.len() - 1);
            }
            if let Ok(line) = str::from_utf8(&buf) {
                if let Some(p) = line.find('=') {
                    self.env
                        .insert(line[..p].to_string(), line[(p + 1)..].to_string());
                } else if let Some(p) = line.find("<<") {
                    let name = line[..p].to_string();
                    let delimiter = line[(p + 2)..].to_string();
                    let mut value = String::new();
                    let mut err = false;
                    loop {
                        let len = env_file.read_until(b'\n', &mut buf).unwrap();
                        if len == 0 {
                            warn!(
                                "Multiline string value not delimited for step {} value named {}",
                                self.step_index.0, name
                            );
                            err = true;
                            break;
                        }
                        debug!(
                            "env line for step {}: {}",
                            self.step_index.0,
                            String::from_utf8_lossy(&buf)
                        );
                        if buf.last() == Some(&b'\n') {
                            buf.truncate(buf.len() - 1);
                        }
                        if buf == delimiter.as_bytes() {
                            break;
                        }
                        if let Ok(s) = str::from_utf8(&buf) {
                            value.push_str(s);
                            value.push('\n');
                        } else {
                            warn!(
                                "Multiline string part not UTF8 for step {} value named {}",
                                self.step_index.0, name
                            );
                            err = true;
                        }
                    }
                    if !err {
                        self.env.insert(name, value);
                    }
                } else {
                    warn!(
                        "No '=' in environment line for step {}: {}",
                        self.step_index.0,
                        String::from_utf8_lossy(&buf)
                    );
                }
            } else {
                warn!(
                    "Non-UTF8 environment line for step {}: {}",
                    self.step_index.0,
                    String::from_utf8_lossy(&buf)
                );
            }
        }
    }

    fn update_path_from_file(&mut self) {
        let mut path_file = BufReader::new(File::open(self.github_path_path()).unwrap());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let len = path_file.read_until(b'\n', &mut buf).unwrap();
            if len == 0 {
                break;
            }
            if buf.last() == Some(&b'\n') {
                buf.truncate(buf.len() - 1);
            }
            if let Ok(path) = str::from_utf8(&buf) {
                self.paths.push(path.to_string());
            } else {
                warn!(
                    "Non-UTF8 path line for step {}: {}",
                    self.step_index.0,
                    String::from_utf8_lossy(&buf)
                );
            }
        }
    }

    async fn configure_action(
        &mut self,
        step: &mut Step,
        action_name: String,
        spec: &mut RunSpec,
    ) -> Result<(), RunnerError> {
        let (action_host_path, action_path) = self.download_action(&action_name).await?;
        let mut action = self.read_action_yaml(&action_name, &action_host_path)?;
        let step_context = StepContext::new(
            &self.ctx,
            &self.matrix_values,
            &self.job_name,
            self.step_index,
        );
        let mut with = step.take_with(&step_context)?;
        let action_context =
            ActionContext::new(&self.ctx, &action_name, &self.job_name, self.step_index);
        for (k, mut v) in action.take_inputs(&action_context)? {
            let value = if let Some(w) = with.remove(&k) {
                w
            } else if let Some(d) = v.take_default(&action_context)? {
                d
            } else {
                continue;
            };
            spec.env.insert(format!("INPUT_{}", envify(&k)), value);
        }
        let mut runs = action.take_runs(&action_context)?;
        if runs.take_pre(&action_context)?.is_some() {
            return Err(RunnerErrorKind::UnsupportedPre {
                action: action_name,
            }
            .into());
        }
        let using = runs.take_using(&action_context)?;
        if using != "node12" {
            return Err(RunnerErrorKind::UnsupportedActionType {
                action: action_name,
                using,
            }
            .into());
        }
        let main = runs.take_main(&action_context)?;

        spec.command = vec![
            "node".to_string(),
            format!("/github/{}/{}", action_path, main),
        ];
        Ok(())
    }
    async fn configure_command(
        &mut self,
        step: &mut Step,
        command: String,
        spec: &mut RunSpec,
    ) -> Result<(), RunnerError> {
        let step_context = StepContext::new(
            &self.ctx,
            &self.matrix_values,
            &self.job_name,
            self.step_index,
        );
        match step.take_shell(&step_context)?.as_deref() {
            None | Some("bash") => {
                spec.command = if let Some(cmd) = parse_simple_shell_command(&command) {
                    cmd
                } else {
                    vec![
                        "bash".to_string(),
                        "--noprofile".to_string(),
                        "--norc".to_string(),
                        "-eo".to_string(),
                        "pipefail".to_string(),
                        "-c".to_string(),
                        command,
                    ]
                };
            }
            Some("sh") => {
                spec.command = if let Some(cmd) = parse_simple_shell_command(&command) {
                    cmd
                } else {
                    vec![
                        "sh".to_string(),
                        "-e".to_string(),
                        "-c".to_string(),
                        command,
                    ]
                };
            }
            Some(shell) => {
                return Err(RunnerErrorKind::UnsupportedShell {
                    shell: shell.to_string(),
                }
                .into())
            }
        }
        Ok(())
    }

    async fn download_action(&self, action: &str) -> Result<(PathBuf, String), RunnerError> {
        let mut action_parts = action.splitn(2, '@').collect::<Vec<_>>();
        if action_parts.len() != 2 {
            return Err(RunnerErrorKind::BadActionName {
                action: action.to_string(),
            }
            .into());
        }
        let (action_ref, action_repo) = (action_parts.pop().unwrap(), action_parts.pop().unwrap());
        let action_repo_parts = action_repo.splitn(2, '/').collect::<Vec<_>>();
        if action_repo_parts.len() != 2 {
            return Err(RunnerErrorKind::BadActionName {
                action: action.to_string(),
            }
            .into());
        }
        let action_path = format!(".gha-runner/step{}/action", self.step_index.0);
        let action_host_path = self.ctx.global_dir_host.join(&action_path);
        let _ = fs::create_dir(&action_host_path);
        let commit: params::repos::Commitish = action_ref.to_string().into();
        let response = self
            .ctx
            .github
            .repos(action_repo_parts[0], action_repo_parts[1])
            .download_tarball(commit)
            .await
            .map_err(|e| RunnerErrorKind::ActionDownloadError {
                action: action.to_string(),
                inner: Box::new(e),
            })?;
        untar_response(action, response.into(), &action_host_path).await?;
        Ok((action_host_path, action_path))
    }

    fn read_action_yaml(&self, action: &str, path: &Path) -> Result<models::Action, RunnerError> {
        let mut file = match File::open(path.join("action.yml")) {
            Ok(f) => Ok(f),
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    File::open(path.join("action.yaml"))
                } else {
                    Err(e)
                }
            }
        }
        .map_err(|_| RunnerErrorKind::ActionDownloadError {
            action: action.to_string(),
            inner: "action.yml not found".into(),
        })?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        Ok(Action(serde_yaml::from_slice(&buf).map_err(|e| {
            RunnerErrorKind::InvalidActionYaml {
                action: action.to_string(),
                inner: e,
            }
        })?))
    }
}

/// Analyzes and runs a specific Github Actions workflow.
pub struct Runner {
    ctx: RunnerContext,
    workflow_env: LinkedHashMap<String, String>,
    workflow_name: Option<String>,
    job_descriptions: Vec<JobDescription>,
}

/// `ErrorContextRoot` describes what we were processing when an error occurred.
#[derive(Clone, Debug)]
pub enum ErrorContextRoot {
    /// The error occurred processing a workflow YAML file.
    Workflow,
    /// The error occurred processing an action's YAML file.
    Action(String),
}

impl fmt::Display for ErrorContextRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            ErrorContextRoot::Workflow => {
                write!(f, "workflow")
            }
            ErrorContextRoot::Action(ref action) => {
                write!(f, "action {}", action)
            }
        }
    }
}

/// This describes wha we know about the source of an error.
#[derive(Clone, Debug)]
pub struct ErrorContext {
    /// Which YAML file was the context of the error.
    pub root: ErrorContextRoot,
    /// Which job we were processing.
    pub job_name: Option<String>,
    /// Which step we were processing.
    pub step: Option<StepIndex>,
}

impl ErrorContext {
    pub(crate) fn new(root: ErrorContextRoot) -> ErrorContext {
        ErrorContext {
            root: root,
            job_name: None,
            step: None,
        }
    }
    pub(crate) fn job_name(mut self, job_name: Option<String>) -> ErrorContext {
        self.job_name = job_name;
        self
    }
    pub(crate) fn step(mut self, step: StepIndex) -> ErrorContext {
        self.step = Some(step);
        self
    }
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        if let Some(job) = self.job_name.as_ref() {
            if let Some(step) = self.step {
                write!(f, "{} job '{}' step {}", self.root, job, step.0)
            } else {
                write!(f, "{} job '{}'", self.root, job)
            }
        } else {
            write!(f, "{}", self.root)
        }
    }
}

/// Details about an error we encountered.
#[derive(Debug)]
pub enum RunnerErrorKind {
    /// Error parsing workflow YAML file.
    InvalidWorkflowYaml {
        /// The actual parse error.
        inner: serde_yaml::Error,
    },
    /// We expected a certain kind of JSON value but found a different kind.
    TypeMismatch {
        /// What we expected.
        expected: String,
        /// What we got.
        got: Value,
    },
    /// A JSON object should have had a specific field but it was missing.
    RequiredFieldMissing {
        /// Name of the missing field.
        field_name: &'static str,
        /// The JSON object.
        got: Value,
    },
    /// Failed to parse a Github Actions expression.
    ExpressionParseError {
        /// The unparseable expression.
        expression: String,
    },
    /// An expression occurs in a string interpolation context but produced a non-string result.
    ExpressionNonString {
        /// The expression.
        expression: String,
        /// The value it evaluated it.
        value: Value,
    },
    /// An 'runs-on' value is not supported.
    UnsupportedPlatform {
        /// The 'runs-on' value.
        runs_on: String,
    },
    /// Failed to parse an action name.
    BadActionName {
        /// The invalid action name.
        action: String,
    },
    /// Failed to download the code for an action.
    ActionDownloadError {
        /// The name of the action.
        action: String,
        /// The download error.
        inner: Box<dyn Error + Sync + Send + 'static>,
    },
    /// Error parsing an action's YAML file.
    InvalidActionYaml {
        /// The name of the action.
        action: String,
        /// The actual parse error.
        inner: serde_yaml::Error,
    },
    /// The workflow specified an environment variable name that's invalid (e.g. contains `=`).
    InvalidEnvironmentVariableName {
        /// The invalid environment variable name.
        name: String,
    },
    /// An action's `using` value is not supported.
    UnsupportedActionType {
        /// The name of the action.
        action: String,
        /// Its unsupported 'using' value.
        using: String,
    },
    /// `pre` is not currently supported.
    UnsupportedPre {
        /// The action that uses `pre`.
        action: String,
    },
    /// A shell type is not supported.
    UnsupportedShell {
        /// The unsupported `shell` value.
        shell: String,
    },
}

impl RunnerErrorKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            RunnerErrorKind::InvalidWorkflowYaml { ref inner, .. } => &*inner,
            RunnerErrorKind::InvalidActionYaml { ref inner, .. } => &*inner,
            RunnerErrorKind::ActionDownloadError { ref inner, .. } => &**inner,
            _ => return None,
        })
    }

    pub(crate) fn error<'a>(self, context: &impl ContextResolver<'a>) -> RunnerError {
        RunnerError {
            kind: self,
            backtrace: Backtrace::new(),
            context: Some(context.error_context()),
        }
    }
}

impl fmt::Display for RunnerErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            RunnerErrorKind::InvalidWorkflowYaml { ref inner } => {
                write!(f, "Invalid workflow YAML: {}", inner)
            }
            RunnerErrorKind::TypeMismatch {
                ref expected,
                ref got,
            } => {
                write!(f, "Expected {}, got {:?}", expected, got)
            }
            RunnerErrorKind::RequiredFieldMissing {
                field_name,
                ref got,
            } => {
                write!(f, "Expected field {}, missing in {:?}", field_name, got)
            }
            RunnerErrorKind::ExpressionParseError { ref expression } => {
                write!(f, "Error parsing expression '{}'", expression)
            }
            RunnerErrorKind::ExpressionNonString {
                ref expression,
                ref value,
            } => {
                write!(
                    f,
                    "Expression in string interpolation '{}' is not a string, got {:?}",
                    expression, value
                )
            }
            RunnerErrorKind::UnsupportedPlatform { ref runs_on } => {
                write!(f, "Platform '{}' not supported", runs_on)
            }
            RunnerErrorKind::BadActionName { ref action } => {
                write!(f, "Cannot parse action name '{}'", action)
            }
            RunnerErrorKind::ActionDownloadError {
                ref action,
                ref inner,
            } => {
                write!(f, "Failed to download action '{}': {}", action, inner)
            }
            RunnerErrorKind::InvalidActionYaml {
                ref action,
                ref inner,
            } => {
                write!(f, "Invalid YAML for action '{}': {}", action, inner)
            }
            RunnerErrorKind::InvalidEnvironmentVariableName { ref name } => {
                write!(f, "Invalid evironment variable name '{}'", name)
            }
            RunnerErrorKind::UnsupportedActionType {
                ref action,
                ref using,
            } => {
                write!(f, "Unsupported type '{}' for action '{}'", using, action)
            }
            RunnerErrorKind::UnsupportedPre { ref action } => {
                write!(f, "'pre' rules not supported for action '{}'", action)
            }
            RunnerErrorKind::UnsupportedShell { ref shell } => {
                write!(f, "Shell '{}' not supported", shell)
            }
        }
    }
}

/// Errors returned by this crate.
#[derive(Debug)]
pub struct RunnerError {
    kind: RunnerErrorKind,
    backtrace: Backtrace,
    context: Option<ErrorContext>,
}

impl RunnerError {
    /// Get error details.
    pub fn kind(&self) -> &RunnerErrorKind {
        &self.kind
    }
    /// Get the error context if there is one.
    pub fn context(&self) -> Option<&ErrorContext> {
        self.context.as_ref()
    }
}

impl From<RunnerErrorKind> for RunnerError {
    fn from(k: RunnerErrorKind) -> RunnerError {
        RunnerError {
            kind: k,
            backtrace: Backtrace::new(),
            context: None,
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        if let Some(ctx) = self.context.as_ref() {
            write!(f, "{} at {} at {:?}", &self.kind, ctx, &self.backtrace)
        } else {
            write!(f, "{} at {:?}", &self.kind, &self.backtrace)
        }
    }
}

impl Error for RunnerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.kind.source()
    }
}

fn cartesian_product(
    keys: &LinkedHashMap<String, Vec<Value>>,
) -> Vec<LinkedHashMap<String, Value>> {
    fn inner(
        v: LinkedHashMap<String, Value>,
        mut keys: linked_hash_map::Iter<String, Vec<Value>>,
        ret: &mut Vec<LinkedHashMap<String, Value>>,
    ) {
        if let Some((k, vals)) = keys.next() {
            for val in vals {
                let mut vv = v.clone();
                vv.insert(k.clone(), val.clone());
                inner(vv, keys.clone(), ret);
            }
        } else {
            ret.push(v);
        }
    }
    let mut ret = Vec::new();
    inner(LinkedHashMap::new(), keys.iter(), &mut ret);
    ret
}

impl Runner {
    /// Create a new runner. `workflow` is the workflow YAML file contents.
    /// Normally you would fetch this from Github but you can instead pass anything
    /// you want here.
    pub async fn new(ctx: RunnerContext, workflow: &[u8]) -> Result<Runner, RunnerError> {
        let mut workflow = Workflow(
            serde_yaml::from_slice(workflow)
                .map_err(|e| RunnerErrorKind::InvalidWorkflowYaml { inner: e })?,
        );
        let mut job_descriptions = Vec::new();
        let root_context = RootContext::new(&ctx, ErrorContextRoot::Workflow);
        let workflow_env = workflow.take_env(&root_context)?;
        for (name, mut job) in workflow.take_jobs(&root_context)? {
            let job_context = JobContext::new(&ctx);
            if let Some(mut strategy) = job.take_strategy(&job_context)? {
                if let Some(mut matrix) = strategy.take_matrix(&job_context)? {
                    let mut values = cartesian_product(&matrix.take_keys(&job_context)?);
                    values.append(&mut matrix.take_include(&job_context)?);
                    if !values.is_empty() {
                        for v in values {
                            let mut context = JobPostStrategyContext::new(&ctx, &v);
                            let runs_on = job.clone_runs_on(&context)?;
                            let name = job.clone_name(&context)?.unwrap_or_else(|| name.clone());
                            context.set_job_name(name.clone());
                            let job_env = job.clone_env(&context)?;
                            let mut derived_name = format!("{} (", name);
                            let mut first = true;
                            for val in v.values() {
                                if let Value::String(ref s) = val {
                                    if first {
                                        first = false;
                                    } else {
                                        derived_name.push_str(", ");
                                    }
                                    derived_name.push_str(&s[..]);
                                } else {
                                    warn!(
                                        "Non-string matrix value, not sure how this gets rendered"
                                    );
                                }
                            }
                            derived_name.push(')');
                            let steps = job.clone_steps(&context)?;
                            job_descriptions.push(JobDescription {
                                name: derived_name,
                                runs_on: runs_on,
                                matrix_values: v,
                                job_env: job_env,
                                steps: steps,
                            });
                        }
                        continue;
                    }
                }
            }
            let empty = LinkedHashMap::new();
            let mut context = JobPostStrategyContext::new(&ctx, &empty);
            let name = job.clone_name(&context)?.unwrap_or(name);
            context.set_job_name(name.clone());
            let steps = job.clone_steps(&context)?;
            job_descriptions.push(JobDescription {
                name: name,
                runs_on: job.clone_runs_on(&context)?,
                matrix_values: LinkedHashMap::new(),
                job_env: job.clone_env(&context)?,
                steps: steps,
            });
        }
        let workflow_name = workflow.take_name(&root_context)?;
        info!(
            "Created Runner for workflow {}",
            workflow_name.as_deref().unwrap_or("<unknown>")
        );
        Ok(Runner {
            ctx: ctx,
            workflow_env: workflow_env,
            workflow_name: workflow_name,
            job_descriptions: job_descriptions,
        })
    }
    /// Get descriptions of all the jobs in the workflow.
    pub fn job_descriptions(&self) -> &[JobDescription] {
        &self.job_descriptions
    }
    /// Create a JobRunner that can be used to run the given job.
    pub async fn job_runner(
        &self,
        description: &JobDescription,
        image_mapping: &DockerImageMapping,
    ) -> Result<JobRunner, RunnerError> {
        let mut images = Vec::new();
        if let Some(image) = description.main_container(image_mapping) {
            images.push(image);
        } else {
            return Err(RunnerErrorKind::UnsupportedPlatform {
                runs_on: description
                    .runs_on
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "<none>".to_string()),
            }
            .into());
        }
        let mut env = self.ctx.create_env(self.workflow_name.clone());

        env.extend(
            self.workflow_env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        env.extend(
            description
                .job_env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        let steps = description.steps.clone();
        Ok(JobRunner {
            ctx: self.ctx.clone(),
            job_name: description.name().to_string(),
            matrix_values: description.matrix_values.clone(),
            container_images: images,
            steps: steps,
            outputs: Vec::new(),
            env: env,
            paths: Vec::new(),
            step_index: StepIndex(0),
        })
    }
}
