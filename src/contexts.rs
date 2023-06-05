use serde_yaml::Value;

use crate::expressions::*;
use crate::*;

/// Implementation of "github" object
pub struct GithubContext<'a> {
    error_context_root: ErrorContextRoot,
    ctx: &'a RunnerContext,
}

impl<'a> ContextResolver<'a> for GithubContext<'a> {
    fn error_context(&self) -> ErrorContext {
        ErrorContext::new(self.error_context_root.clone())
    }
    fn keys(&self) -> Vec<String> {
        vec!["repository".to_string(), "token".to_string()]
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "repository" => Some(format!("{}/{}", self.ctx.owner, self.ctx.repo).into()),
            // Return an empty string for github.token, for now. We should try
            // to extract a fresh app installation token from `github` Octocrab.
            "token" => Some(self.ctx.token.clone().into()),
            // Return something for github.event_name.
            "event_name" => Some("pernosco".to_string().into()),
            _ => None,
        }
    }
}

pub struct DummyContext<'a> {
    error_context_root: ErrorContextRoot,
    _ctx: &'a RunnerContext,
}

impl<'a> ContextResolver<'a> for DummyContext<'a> {
    fn error_context(&self) -> ErrorContext {
        ErrorContext::new(self.error_context_root.clone())
    }
    fn keys(&self) -> Vec<String> {
        vec![]
    }
    fn resolve(&self, _s: &str) -> Option<ResolverValue<'a>> {
        None
    }
}

/// Implementation of "strategy" object (dummy for now)
pub struct MatrixContext<'a> {
    matrix_values: &'a LinkedHashMap<String, Value>,
}

impl<'a> ContextResolver<'a> for MatrixContext<'a> {
    fn error_context(&self) -> ErrorContext {
        ErrorContext::new(ErrorContextRoot::Workflow)
    }
    fn keys(&self) -> Vec<String> {
        self.matrix_values.keys().cloned().collect()
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        self.matrix_values
            .get(s)
            .map(|v| ResolverValue::Yaml(v.clone()))
    }
}

/// Context objects that can be used anywhere in the workflow description
pub struct RootContext<'a> {
    error_context_root: ErrorContextRoot,
    ctx: &'a RunnerContext,
}

impl<'a> RootContext<'a> {
    pub fn new(ctx: &'a RunnerContext, error_context_root: ErrorContextRoot) -> Self {
        RootContext {
            error_context_root: error_context_root,
            ctx: ctx,
        }
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.ctx
    }
}

impl<'a> ContextResolver<'a> for RootContext<'a> {
    fn error_context(&self) -> ErrorContext {
        ErrorContext::new(self.error_context_root.clone())
    }
    fn keys(&self) -> Vec<String> {
        vec!["github".to_string()]
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "github" => Some(ResolverValue::Context(Box::new(GithubContext {
                error_context_root: self.error_context_root.clone(),
                ctx: self.runner_context(),
            }))),
            _ => None,
        }
    }
}

/// Context objects that can be used anywhere in a workflow job
pub struct JobContext<'a> {
    parent: RootContext<'a>,
}

impl<'a> JobContext<'a> {
    pub fn new(ctx: &'a RunnerContext) -> Self {
        JobContext {
            parent: RootContext::new(ctx, ErrorContextRoot::Workflow),
        }
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.parent.runner_context()
    }
}

impl<'a> ContextResolver<'a> for JobContext<'a> {
    fn error_context(&self) -> ErrorContext {
        self.parent.error_context()
    }
    fn keys(&self) -> Vec<String> {
        let mut ret = self.parent.keys();
        ret.push("needs".to_string());
        ret
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "needs" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: self.parent.error_context_root.clone(),
                _ctx: self.runner_context(),
            }))),
            _ => self.parent.resolve(s),
        }
    }
}

/// Context objects that can be used anywhere in a workflow job outside the strategy
pub struct JobPostStrategyContext<'a> {
    parent: JobContext<'a>,
    matrix_values: &'a LinkedHashMap<String, Value>,
    job_name: Option<String>,
}

impl<'a> JobPostStrategyContext<'a> {
    pub fn new(ctx: &'a RunnerContext, matrix_values: &'a LinkedHashMap<String, Value>) -> Self {
        JobPostStrategyContext {
            parent: JobContext::new(ctx),
            matrix_values: matrix_values,
            job_name: None,
        }
    }
    pub fn set_job_name(&mut self, v: String) {
        self.job_name = Some(v);
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.parent.runner_context()
    }
}

impl<'a> ContextResolver<'a> for JobPostStrategyContext<'a> {
    fn error_context(&self) -> ErrorContext {
        self.parent.error_context().job_name(self.job_name.clone())
    }
    fn keys(&self) -> Vec<String> {
        let mut ret = self.parent.keys();
        ret.push("strategy".to_string());
        ret.push("matrix".to_string());
        ret
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "strategy" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: ErrorContextRoot::Workflow,
                _ctx: self.runner_context(),
            }))),
            "matrix" => Some(ResolverValue::Context(Box::new(MatrixContext {
                matrix_values: self.matrix_values,
            }))),
            _ => self.parent.resolve(s),
        }
    }
}

/// Context objects that can be used in a build step before any previous steps have run
pub struct PreStepContext<'a> {
    parent: JobPostStrategyContext<'a>,
    step: StepIndex,
}

impl<'a> PreStepContext<'a> {
    pub fn new(
        ctx: &'a RunnerContext,
        matrix_values: &'a LinkedHashMap<String, Value>,
        job_name: &str,
        step: StepIndex,
    ) -> Self {
        let mut parent = JobPostStrategyContext::new(ctx, matrix_values);
        parent.set_job_name(job_name.to_string());
        PreStepContext {
            parent: parent,
            step: step,
        }
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.parent.runner_context()
    }
}

impl<'a> ContextResolver<'a> for PreStepContext<'a> {
    fn error_context(&self) -> ErrorContext {
        self.parent.error_context().step(self.step)
    }
    fn keys(&self) -> Vec<String> {
        let mut ret = self.parent.keys();
        ret.push("job".to_string());
        ret.push("runner".to_string());
        ret
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "job" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: ErrorContextRoot::Workflow,
                _ctx: self.runner_context(),
            }))),
            "runner" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: ErrorContextRoot::Workflow,
                _ctx: self.runner_context(),
            }))),
            _ => self.parent.resolve(s),
        }
    }
}

/// Context objects that can be used in a build step after previous steps have run
pub struct StepContext<'a> {
    parent: PreStepContext<'a>,
}

impl<'a> StepContext<'a> {
    pub fn new(
        ctx: &'a RunnerContext,
        matrix_values: &'a LinkedHashMap<String, Value>,
        job_name: &str,
        step: StepIndex,
    ) -> Self {
        StepContext {
            parent: PreStepContext::new(ctx, matrix_values, job_name, step),
        }
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.parent.runner_context()
    }
}

impl<'a> ContextResolver<'a> for StepContext<'a> {
    fn error_context(&self) -> ErrorContext {
        self.parent.error_context()
    }
    fn keys(&self) -> Vec<String> {
        let mut ret = self.parent.keys();
        ret.push("steps".to_string());
        ret.push("env".to_string());
        ret
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "steps" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: ErrorContextRoot::Workflow,
                _ctx: self.runner_context(),
            }))),
            "env" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: ErrorContextRoot::Workflow,
                _ctx: self.runner_context(),
            }))),
            _ => self.parent.resolve(s),
        }
    }
}

/// Context objects that can be used anywhere in an action
pub struct ActionContext<'a> {
    parent: RootContext<'a>,
    job_name: String,
    step: StepIndex,
}

impl<'a> ActionContext<'a> {
    pub fn new(ctx: &'a RunnerContext, action: &str, job_name: &str, step: StepIndex) -> Self {
        ActionContext {
            parent: RootContext::new(ctx, ErrorContextRoot::Action(action.to_string())),
            job_name: job_name.to_string(),
            step: step,
        }
    }
    pub fn runner_context(&self) -> &'a RunnerContext {
        self.parent.runner_context()
    }
}

impl<'a> ContextResolver<'a> for ActionContext<'a> {
    fn error_context(&self) -> ErrorContext {
        self.parent
            .error_context()
            .job_name(Some(self.job_name.clone()))
            .step(self.step)
    }
    fn keys(&self) -> Vec<String> {
        let mut ret = self.parent.keys();
        ret.push("runner".to_string());
        ret
    }
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>> {
        match s {
            "runner" => Some(ResolverValue::Context(Box::new(DummyContext {
                error_context_root: self.parent.error_context_root.clone(),
                _ctx: self.runner_context(),
            }))),
            _ => self.parent.resolve(s),
        }
    }
}
