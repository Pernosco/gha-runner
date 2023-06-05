use serde_yaml::Value;

use super::*;

pub enum ResolverValue<'a> {
    Yaml(Value),
    Context(Box<dyn ContextResolver<'a>>),
}

impl<'a> From<String> for ResolverValue<'a> {
    fn from(v: String) -> Self {
        ResolverValue::Yaml(Value::String(v))
    }
}

pub trait ContextResolver<'a>: Send + Sync + 'a {
    fn error_context(&self) -> ErrorContext;
    fn keys(&self) -> Vec<String>;
    fn resolve(&self, s: &str) -> Option<ResolverValue<'a>>;
}

fn is_context_identifier_char(ch: char) -> bool {
    ('a'..='z').contains(&ch)
}

fn is_context_field_char(ch: char) -> bool {
    matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_')
}

fn evaluate_context_name(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|ch| !is_context_identifier_char(ch))
        .unwrap_or_else(|| s.len());
    if end > 0 {
        Some((&s[..end], s[end..].trim_start()))
    } else {
        None
    }
}

fn evaluate_context_field(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|ch| !is_context_field_char(ch))
        .unwrap_or_else(|| s.len());
    if end > 0 {
        Some((&s[..end], s[end..].trim_start()))
    } else {
        None
    }
}

fn get_context_value<'a>(
    resolver: &'a dyn ContextResolver,
    base: &str,
    components: Vec<&str>,
) -> Option<ResolverValue<'a>> {
    let mut resolver_value: ResolverValue<'a> = resolver.resolve(base)?;

    for c in components {
        match resolver_value {
            ResolverValue::Yaml(yaml) => {
                if let Some(v) = yaml.get(&c) {
                    resolver_value = ResolverValue::Yaml(v.clone());
                } else {
                    return None;
                }
            }
            ResolverValue::Context(ctx) => {
                if let Some(r) = ctx.resolve(c) {
                    resolver_value = r;
                } else {
                    return None;
                }
            }
        }
    }
    Some(resolver_value)
}

fn evaluate_context_expression<'a, 'b>(
    resolver: &'a dyn ContextResolver,
    s: &'b str,
) -> Result<Option<(ResolverValue<'a>, &'b str)>, RunnerError> {
    if let Some((name, mut rest)) = evaluate_context_name(s) {
        rest = rest.trim_start();
        let mut components = Vec::new();
        loop {
            if rest.starts_with('.') {
                if let Some((field, rest2)) = evaluate_context_field(&rest[1..]) {
                    components.push(field);
                    rest = rest2;
                    continue;
                }
            }
            if rest.starts_with('[') {
                unimplemented!("Not supported yet");
            } else {
                break;
            }
        }
        Ok(get_context_value(resolver, name, components).map(|v| (v, rest)))
    } else {
        Ok(None)
    }
}

fn evaluate<'a>(
    resolver: &'a dyn ContextResolver,
    mut s: &str,
) -> Result<ResolverValue<'a>, RunnerError> {
    s = s.trim_start();
    if let Some((v, mut rest)) = evaluate_context_expression(resolver, s)? {
        rest = rest.trim_start();
        if rest.is_empty() {
            Ok(v)
        } else {
            Err(RunnerErrorKind::ExpressionParseError {
                expression: s.to_string(),
            }
            .into())
        }
    } else {
        Err(RunnerErrorKind::ExpressionParseError {
            expression: s.to_string(),
        }
        .into())
    }
}

/// If 'value' is a string, evaluate any expressions in it and return
/// the result. This can be a complex YAML object (e.g. due to fromJSON).
/// If 'value' is a sequence, build a new sequence with its contents
/// expanded.
pub fn expand(resolver: &dyn ContextResolver, value: Value) -> Result<Value, RunnerError> {
    let s = match value {
        Value::String(s) => s,
        Value::Sequence(seq) => {
            let mut ret = Vec::new();
            for v in seq {
                ret.push(expand(resolver, v)?);
            }
            return Ok(Value::Sequence(ret));
        }
        _ => return Ok(value),
    };
    let mut slice = &s[..];
    let mut ret = String::new();
    while let Some(p) = slice.find("${{") {
        let rest = &slice[(p + 3)..];
        if let Some(len) = rest.find("}}") {
            ret.push_str(&slice[..p]);
            slice = &rest[(len + 2)..];
            let value = match evaluate(resolver, &rest[..len])? {
                ResolverValue::Yaml(v) => v,
                ResolverValue::Context(_) => {
                    // XXX replace this with code to walk the resolver keys and build a YAML version of it
                    panic!("Tried to pass a context object as the result of the expression, we don't support this yet");
                }
            };
            if ret.is_empty() && slice.is_empty() {
                // This expression stands alone, so let it return structured YAML.
                return Ok(value);
            }
            // OK, we're doing string interpolation
            match value {
                Value::String(ref s) => ret.push_str(s),
                Value::Bool(true) => ret.push_str("true"),
                Value::Bool(false) => ret.push_str("false"),
                _ => {
                    return Err(RunnerErrorKind::ExpressionNonString {
                        expression: rest[..len].to_string(),
                        value: value,
                    }
                               .into());
                }
            }
        } else {
            break;
        }
    }
    ret.push_str(slice);
    Ok(Value::String(ret))
}
