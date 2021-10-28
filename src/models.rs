use std::mem;

use linked_hash_map::LinkedHashMap;
use serde_yaml::{Number, Value};

use crate::contexts::*;
use crate::expressions::*;
use crate::{RunnerError, RunnerErrorKind};

#[derive(Clone)]
/// The Value has already been expanded.
pub struct Strategy(pub Value);

/// The Value has already been expanded.
#[derive(Clone)]
pub struct Matrix(pub Value);

#[derive(Clone)]
/// The Value has already been expanded.
pub struct Job(pub Value);

#[derive(Clone)]
/// The Value has already been expanded.
pub struct Step(pub Value);

/// The Value has already been expanded (actually we assume the whole Workflow
/// is never an expression.)
pub struct Workflow(pub Value);

/// Converts the YAML value to a string-keyed map, expanding
/// the values in the map before passing them to the processing function `f`.
fn to_string_map<'a, F, V, C: ContextResolver<'a>>(
    context: &'a C,
    yaml: Value,
    mut f: F,
) -> Result<LinkedHashMap<String, V>, RunnerError>
where
    F: FnMut(&C, Value) -> Result<V, RunnerError>,
{
    if let Value::Mapping(m) = yaml {
        let mut ret = LinkedHashMap::new();
        for (k, v) in m {
            if let Value::String(s) = k {
                ret.insert(s, f(context, expand(context, v)?)?);
            } else {
                return Err(RunnerErrorKind::TypeMismatch {
                    expected: "string key".to_string(),
                    got: v,
                }
                .error(context));
            }
        }
        return Ok(ret);
    }
    Err(RunnerErrorKind::TypeMismatch {
        expected: "mapping".to_string(),
        got: yaml,
    }
    .error(context))
}

fn num_to_string(n: Number) -> String {
    if let Some(v) = n.as_i64() {
        v.to_string()
    } else if let Some(v) = n.as_u64() {
        v.to_string()
    } else {
        panic!("Unsupported number {:?}", n);
    }
}

fn to_string<'a>(context: &impl ContextResolver<'a>, yaml: Value) -> Result<String, RunnerError> {
    Ok(match yaml {
        Value::String(s) => s,
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => num_to_string(n),
        yaml => {
            return Err(RunnerErrorKind::TypeMismatch {
                expected: "string".to_string(),
                got: yaml,
            }
            .error(context));
        }
    })
}

fn to_string_opt<'a>(
    context: &impl ContextResolver<'a>,
    yaml: Option<Value>,
) -> Result<Option<String>, RunnerError> {
    if let Some(yaml) = yaml {
        Ok(Some(to_string(context, yaml)?))
    } else {
        Ok(None)
    }
}

/// Takes the field value and also applies expansion.
fn take_field<'a, C: ContextResolver<'a>>(
    resolver: &'a C,
    value: &mut Value,
    field_name: &'static str,
) -> Result<Option<Value>, RunnerError> {
    if let Some(mapping) = value.as_mapping_mut() {
        let key = Value::String(field_name.to_string());
        if let Some(val) = mapping.remove(&key) {
            return expand(resolver, val).map(Some);
        }
        return Ok(None);
    }
    Err(RunnerErrorKind::TypeMismatch {
        expected: "mapping".to_string(),
        got: value.clone(),
    }
    .into())
}

/// Clone the field value and also applies expansion.
fn take_required_field<'a, C: ContextResolver<'a>>(
    resolver: &'a C,
    value: &mut Value,
    field_name: &'static str,
) -> Result<Value, RunnerError> {
    if let Some(ret) = take_field(resolver, value, field_name)? {
        Ok(ret)
    } else {
        Err(RunnerErrorKind::RequiredFieldMissing {
            field_name,
            got: value.clone(),
        }
        .into())
    }
}

/// Clone the field value and also applies expansion.
fn clone_field<'a, C: ContextResolver<'a>>(
    resolver: &'a C,
    value: &Value,
    field_name: &'static str,
) -> Result<Option<Value>, RunnerError> {
    if let Some(mapping) = value.as_mapping() {
        let key = Value::String(field_name.to_string());
        if let Some(val) = mapping.get(&key) {
            return expand(resolver, val.clone()).map(Some);
        }
        return Ok(None);
    }
    Err(RunnerErrorKind::TypeMismatch {
        expected: "mapping".to_string(),
        got: value.clone(),
    }
    .error(resolver))
}

/// Clone the field value and also applies expansion.
fn clone_required_field<'a, C: ContextResolver<'a>>(
    resolver: &'a C,
    value: &Value,
    field_name: &'static str,
) -> Result<Value, RunnerError> {
    if let Some(ret) = clone_field(resolver, value, field_name)? {
        Ok(ret)
    } else {
        Err(RunnerErrorKind::RequiredFieldMissing {
            field_name,
            got: value.clone(),
        }
        .error(resolver))
    }
}

impl Workflow {
    pub fn take_name(&mut self, context: &RootContext) -> Result<Option<String>, RunnerError> {
        to_string_opt(context, take_field(context, &mut self.0, "name")?)
    }
    pub fn take_env(
        &mut self,
        context: &RootContext,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "env")? {
            to_string_map(context, yaml, to_string)
        } else {
            Ok(LinkedHashMap::new())
        }
    }
    pub fn take_jobs(
        &mut self,
        context: &RootContext,
    ) -> Result<LinkedHashMap<String, Job>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "jobs")? {
            to_string_map(context, yaml, |_, v| Ok(Job(v)))
        } else {
            Ok(LinkedHashMap::new())
        }
    }
}

impl Matrix {
    pub fn take_keys<'a>(
        &mut self,
        context: &'a JobContext<'a>,
    ) -> Result<LinkedHashMap<String, Vec<Value>>, RunnerError> {
        if let Some(mapping) = self.0.as_mapping_mut() {
            let mut ret = LinkedHashMap::new();
            let all = mem::take(mapping);
            for (k, v) in all {
                if let Value::String(s) = k {
                    if s == "include" {
                        mapping.insert(Value::String(s), v);
                        continue;
                    }
                    let expanded = expand(context, v)?;
                    if let Value::Sequence(seq) = expanded {
                        ret.insert(s, seq);
                    } else {
                        return Err(RunnerErrorKind::TypeMismatch {
                            expected: "matrix list of values".to_string(),
                            got: expanded,
                        }
                        .error(context));
                    }
                } else {
                    return Err(RunnerErrorKind::TypeMismatch {
                        expected: "string key".to_string(),
                        got: v,
                    }
                    .error(context));
                }
            }
            return Ok(ret);
        }
        Err(RunnerErrorKind::TypeMismatch {
            expected: "mapping".to_string(),
            got: self.0.clone(),
        }
        .error(context))
    }
    pub fn take_include(
        &mut self,
        context: &JobContext,
    ) -> Result<Vec<LinkedHashMap<String, Value>>, RunnerError> {
        if let Some(include) = take_field(context, &mut self.0, "include")? {
            if let Value::Sequence(seq) = include {
                let mut ret = Vec::new();
                for v in seq {
                    ret.push(to_string_map(context, v, |_, v| Ok(v))?);
                }
                Ok(ret)
            } else {
                Err(RunnerErrorKind::TypeMismatch {
                    expected: "sequence of includes".to_string(),
                    got: include,
                }
                .error(context))
            }
        } else {
            Ok(Vec::new())
        }
    }
}

impl Strategy {
    pub fn take_matrix(&mut self, context: &JobContext) -> Result<Option<Matrix>, RunnerError> {
        take_field(context, &mut self.0, "matrix").map(|v| v.map(Matrix))
    }
}

impl Step {
    pub fn take_name_pre(
        &mut self,
        context: &PreStepContext,
    ) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "name").and_then(|v| to_string_opt(context, v))
    }
    pub fn take_name(&mut self, context: &StepContext) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "name").and_then(|v| to_string_opt(context, v))
    }
    pub fn take_uses(&mut self, context: &StepContext) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "uses").and_then(|v| to_string_opt(context, v))
    }
    pub fn take_run(&mut self, context: &StepContext) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "run").and_then(|v| to_string_opt(context, v))
    }
    pub fn take_shell(&mut self, context: &StepContext) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "shell").and_then(|v| to_string_opt(context, v))
    }
    pub fn take_working_directory(
        &mut self,
        context: &StepContext,
    ) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "working-directory")
            .and_then(|v| to_string_opt(context, v))
    }
    pub fn take_env(
        &mut self,
        context: &StepContext,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "env")? {
            to_string_map(context, yaml, to_string)
        } else {
            Ok(LinkedHashMap::new())
        }
    }
    pub fn take_env_pre(
        &mut self,
        context: &PreStepContext,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "env")? {
            to_string_map(context, yaml, to_string)
        } else {
            Ok(LinkedHashMap::new())
        }
    }
    pub fn take_with(
        &mut self,
        context: &StepContext,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "with")? {
            to_string_map(context, yaml, to_string)
        } else {
            Ok(LinkedHashMap::new())
        }
    }
}

impl Job {
    pub fn take_strategy(&mut self, context: &JobContext) -> Result<Option<Strategy>, RunnerError> {
        take_field(context, &mut self.0, "strategy").map(|v| v.map(Strategy))
    }
    pub fn clone_env(
        &self,
        context: &JobPostStrategyContext,
    ) -> Result<LinkedHashMap<String, String>, RunnerError> {
        if let Some(yaml) = clone_field(context, &self.0, "env")? {
            to_string_map(context, yaml, to_string)
        } else {
            Ok(LinkedHashMap::new())
        }
    }
    pub fn clone_name(
        &self,
        context: &JobPostStrategyContext,
    ) -> Result<Option<String>, RunnerError> {
        if let Some(yaml) = clone_field(context, &self.0, "name")? {
            Ok(Some(to_string(context, yaml)?))
        } else {
            Ok(None)
        }
    }
    pub fn clone_runs_on(
        &self,
        context: &JobPostStrategyContext,
    ) -> Result<Vec<String>, RunnerError> {
        let yaml = clone_required_field(context, &self.0, "runs-on")?;
        Ok(match yaml {
            Value::String(s) => vec![s],
            Value::Sequence(seq) => {
                let mut ret = Vec::new();
                for v in seq {
                    ret.push(to_string(context, v)?);
                }
                ret
            }
            _ => {
                return Err(RunnerErrorKind::TypeMismatch {
                    expected: "String or array of strings".to_string(),
                    got: yaml,
                }
                .error(context))
            }
        })
    }
    pub fn clone_steps(&self, context: &JobPostStrategyContext) -> Result<Vec<Step>, RunnerError> {
        let yaml = clone_required_field(context, &self.0, "steps")?;
        Ok(if let Value::Sequence(seq) = yaml {
            seq.into_iter().map(Step).collect()
        } else {
            return Err(RunnerErrorKind::TypeMismatch {
                expected: "Array of steps".to_string(),
                got: yaml,
            }
            .error(context));
        })
    }
}

// action.yml data

/// The Value has already been expanded
pub struct Input(pub Value);

impl Input {
    pub fn take_default(&mut self, context: &ActionContext) -> Result<Option<String>, RunnerError> {
        if let Some(yaml) = take_field(context, &mut self.0, "default")? {
            Ok(Some(to_string(context, yaml)?))
        } else {
            Ok(None)
        }
    }
}

/// The Value has already been expanded
pub struct Runs(pub Value);

impl Runs {
    pub fn take_using(&mut self, context: &ActionContext) -> Result<String, RunnerError> {
        take_required_field(context, &mut self.0, "using").and_then(|v| to_string(context, v))
    }
    pub fn take_main(&mut self, context: &ActionContext) -> Result<String, RunnerError> {
        take_required_field(context, &mut self.0, "main").and_then(|v| to_string(context, v))
    }
    pub fn take_pre(&mut self, context: &ActionContext) -> Result<Option<String>, RunnerError> {
        take_field(context, &mut self.0, "pre").and_then(|v| to_string_opt(context, v))
    }
}

/// The Value has already been expanded (actually we assume the whole Action
/// is never an expression.)
pub struct Action(pub Value);

impl Action {
    pub fn take_inputs(
        &mut self,
        context: &ActionContext,
    ) -> Result<LinkedHashMap<String, Input>, RunnerError> {
        if let Some(mut inputs) = take_field(context, &mut self.0, "inputs")? {
            if let Some(mapping) = inputs.as_mapping_mut() {
                let mut ret = LinkedHashMap::new();
                let all = mem::take(mapping);
                for (k, v) in all {
                    if let Value::String(s) = k {
                        ret.insert(s, Input(v));
                    } else {
                        return Err(RunnerErrorKind::TypeMismatch {
                            expected: "string key".to_string(),
                            got: v,
                        }
                        .error(context));
                    }
                }
                return Ok(ret);
            }
            Err(RunnerErrorKind::TypeMismatch {
                expected: "mapping".to_string(),
                got: self.0.clone(),
            }
            .error(context))
        } else {
            Ok(LinkedHashMap::new())
        }
    }
    pub fn take_runs(&mut self, context: &ActionContext) -> Result<Runs, RunnerError> {
        take_required_field(context, &mut self.0, "runs").map(Runs)
    }
}
