use cel_interpreter::{Context, Program, Value};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CelResult {
    pub passed: bool,
    pub expression: String,
    pub error: Option<String>,
}

pub fn evaluate(expression: &str, variables: &CelContext) -> CelResult {
    let program = match Program::compile(expression) {
        Ok(p) => p,
        Err(e) => {
            return CelResult {
                passed: false,
                expression: expression.to_string(),
                error: Some(format!("CEL compile error: {}", e)),
            };
        }
    };

    let mut context = Context::default();
    populate_context(&mut context, variables);

    match program.execute(&context) {
        Ok(Value::Bool(b)) => CelResult {
            passed: b,
            expression: expression.to_string(),
            error: None,
        },
        Ok(other) => CelResult {
            passed: false,
            expression: expression.to_string(),
            error: Some(format!(
                "CEL expression must evaluate to bool, got: {:?}",
                other
            )),
        },
        Err(e) => CelResult {
            passed: false,
            expression: expression.to_string(),
            error: Some(format!("CEL evaluation error: {}", e)),
        },
    }
}

/// Variables available in a CEL evaluation context.
#[derive(Debug, Clone, Default)]
pub struct CelContext {
    pub message: Option<MessageContext>,
    pub trajectory: Option<TrajectoryContext>,
}

#[derive(Debug, Clone, Default)]
pub struct MessageContext {
    pub text: String,
    pub tool_calls: Vec<ToolCallContext>,
    pub reasoning: Option<String>,
    pub reaction: Option<String>,
    pub reply_to: Option<i64>,
    pub format: Option<String>,
    pub inline_keyboard: Option<Vec<HashMap<String, String>>>,
}

#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub name: String,
    pub arguments: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct TrajectoryContext {
    pub assistant_count: i64,
    pub user_count: i64,
    pub tool_call_count: i64,
}

fn populate_context(context: &mut Context, variables: &CelContext) {
    if let Some(msg) = &variables.message {
        let mut message_map: HashMap<Arc<String>, Value> = HashMap::new();

        message_map.insert(
            Arc::new("text".to_string()),
            Value::String(Arc::new(msg.text.clone())),
        );

        let tool_calls: Vec<Value> = msg
            .tool_calls
            .iter()
            .map(|tc| {
                let mut tc_map: HashMap<Arc<String>, Value> = HashMap::new();
                tc_map.insert(
                    Arc::new("name".to_string()),
                    Value::String(Arc::new(tc.name.clone())),
                );
                let args: HashMap<Arc<String>, Value> = tc
                    .arguments
                    .iter()
                    .map(|(k, v)| (Arc::new(k.clone()), json_to_cel(v)))
                    .collect();
                tc_map.insert(Arc::new("arguments".to_string()), Value::Map(args.into()));
                Value::Map(tc_map.into())
            })
            .collect();
        message_map.insert(
            Arc::new("tool_calls".to_string()),
            Value::List(Arc::new(tool_calls)),
        );

        if let Some(reasoning) = &msg.reasoning {
            message_map.insert(
                Arc::new("reasoning".to_string()),
                Value::String(Arc::new(reasoning.clone())),
            );
        }

        if let Some(reaction) = &msg.reaction {
            message_map.insert(
                Arc::new("reaction".to_string()),
                Value::String(Arc::new(reaction.clone())),
            );
        }

        if let Some(reply_to) = msg.reply_to {
            message_map.insert(Arc::new("reply_to".to_string()), Value::Int(reply_to));
        }

        if let Some(fmt) = &msg.format {
            message_map.insert(
                Arc::new("format".to_string()),
                Value::String(Arc::new(fmt.clone())),
            );
        }

        if let Some(keyboard) = &msg.inline_keyboard {
            let kb_val: Vec<Value> = keyboard
                .iter()
                .map(|row| {
                    let row_map: HashMap<Arc<String>, Value> = row
                        .iter()
                        .map(|(k, v)| {
                            (
                                Arc::new(k.clone()),
                                Value::String(Arc::new(v.clone())),
                            )
                        })
                        .collect();
                    Value::Map(row_map.into())
                })
                .collect();
            message_map.insert(
                Arc::new("inline_keyboard".to_string()),
                Value::List(Arc::new(kb_val)),
            );
        }

        context
            .add_variable("message", Value::Map(message_map.into()))
            .ok();
    }

    if let Some(traj) = &variables.trajectory {
        let mut traj_map: HashMap<Arc<String>, Value> = HashMap::new();
        traj_map.insert(
            Arc::new("assistant_count".to_string()),
            Value::Int(traj.assistant_count),
        );
        traj_map.insert(
            Arc::new("user_count".to_string()),
            Value::Int(traj.user_count),
        );
        traj_map.insert(
            Arc::new("tool_call_count".to_string()),
            Value::Int(traj.tool_call_count),
        );
        context
            .add_variable("trajectory", Value::Map(traj_map.into()))
            .ok();
    }
}

fn json_to_cel(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                Value::UInt(u)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(Arc::new(s.clone())),
        serde_json::Value::Array(arr) => {
            Value::List(Arc::new(arr.iter().map(json_to_cel).collect()))
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<Arc<String>, Value> = obj
                .iter()
                .map(|(k, v)| (Arc::new(k.clone()), json_to_cel(v)))
                .collect();
            Value::Map(map.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_text_contains() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: "The answer is 838102050".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.text.contains(\"838102050\")", &ctx);
        assert!(result.passed);
        assert!(result.error.is_none());
    }

    #[test]
    fn evaluates_text_contains_negative() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: "Hello world".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.text.contains(\"838102050\")", &ctx);
        assert!(!result.passed);
        assert!(result.error.is_none());
    }

    #[test]
    fn evaluates_tool_call_exists() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: String::new(),
                tool_calls: vec![ToolCallContext {
                    name: "bash".to_string(),
                    arguments: HashMap::new(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate(
            "message.tool_calls.exists(t, t.name == \"bash\")",
            &ctx,
        );
        assert!(result.passed);
    }

    #[test]
    fn evaluates_tool_call_size() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: String::new(),
                tool_calls: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.tool_calls.size() == 0", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn evaluates_reply_to() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: String::new(),
                reply_to: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.reply_to == 3", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn evaluates_reaction() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: String::new(),
                reaction: Some("👍".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.reaction == \"👍\"", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn reports_compile_error() {
        let ctx = CelContext::default();
        let result = evaluate("this is not valid cel !!!", &ctx);
        assert!(!result.passed);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("compile error"));
    }

    #[test]
    fn reports_non_bool_result() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: "hello".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.text", &ctx);
        assert!(!result.passed);
        assert!(result.error.unwrap().contains("must evaluate to bool"));
    }

    #[test]
    fn evaluates_trajectory_context() {
        let ctx = CelContext {
            trajectory: Some(TrajectoryContext {
                assistant_count: 3,
                user_count: 2,
                tool_call_count: 5,
            }),
            ..Default::default()
        };
        let result = evaluate("trajectory.assistant_count == 3", &ctx);
        assert!(result.passed);
    }

    #[test]
    fn evaluates_regex_match() {
        let ctx = CelContext {
            message: Some(MessageContext {
                text: "The result is 838102050 exactly".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate("message.text.matches(\"[0-9]{9,}\")", &ctx);
        assert!(result.passed);
    }
}
