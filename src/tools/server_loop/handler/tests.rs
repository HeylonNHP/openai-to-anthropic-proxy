use super::*;

/// A tool that relies on every default.
struct Bare;

impl ServerTool for Bare {
    fn executor_name(&self) -> &'static str {
        "bare"
    }

    fn serve<'a>(&'a mut self, _: &'a Attempt, _: &'a Call) -> BoxFuture<'a, Served> {
        Box::pin(async {
            Served {
                blocks: Vec::new(),
                output: String::new(),
            }
        })
    }
}

#[test]
fn the_defaults_change_nothing() {
    let mut tool = Bare;
    let mut prefix = Vec::new();
    tool.on_rerun_failure(&mut prefix);
    assert!(prefix.is_empty());

    let mut usage = crate::anthropic::Usage {
        input_tokens: 1,
        output_tokens: 2,
        ..crate::anthropic::Usage::default()
    };
    tool.contribute_usage(&mut usage);
    assert!(usage.server_tool_use.is_none() && usage.iterations.is_none());

    let err = tool.scrub_error(AppError::BadRequest("keep me".into()));
    assert!(matches!(err, AppError::BadRequest(m) if m == "keep me"));
}
