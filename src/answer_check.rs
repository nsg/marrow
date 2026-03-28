use std::error::Error;

use crate::model::ModelBackend;

const CHECK_PROMPT_TEMPLATE: &str = r#"You are a quality checker. A tool-assisted agent was given a task and produced a response. Determine whether the response actually answers the task, or if it indicates insufficient data.

Task: {task}

Tool output provided to the agent:
{tool_output}

Agent's response:
{response}

Did the agent successfully answer the task?
- YES if the response contains a substantive answer to what was asked
- NO if the response says it cannot answer, lacks data, or the answer is clearly wrong/incomplete

Respond in this exact format:
```verdict
YES or NO
```
```reason
<one line explaining why, especially what's missing if NO>
```"#;

#[derive(Debug)]
pub struct CheckResult {
    pub answered: bool,
    pub reason: String,
}

pub async fn check_answer(
    task_description: &str,
    tool_output: &str,
    response: &str,
    backend: &dyn ModelBackend,
) -> Result<CheckResult, Box<dyn Error + Send + Sync>> {
    let prompt = CHECK_PROMPT_TEMPLATE
        .replace("{task}", task_description)
        .replace("{tool_output}", tool_output)
        .replace("{response}", response);

    let model_response = backend.complete(prompt).await?;
    parse_check_response(&model_response)
}

fn parse_check_response(response: &str) -> Result<CheckResult, Box<dyn Error + Send + Sync>> {
    let verdict = extract_block(response, "verdict")
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    let reason = extract_block(response, "reason")
        .unwrap_or_else(|| "unknown".to_string())
        .trim()
        .to_string();

    Ok(CheckResult {
        answered: verdict.contains("YES"),
        reason,
    })
}

fn extract_block(response: &str, tag: &str) -> Option<String> {
    let start_marker = format!("```{tag}");
    let start = response.find(&start_marker)?;
    let content_start = start + start_marker.len();
    let rest = &response[content_start..];
    let newline = rest.find('\n')?;
    let rest = &rest[newline + 1..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yes_verdict() {
        let input = "```verdict\nYES\n```\n```reason\nThe response answers the question.\n```";
        let r = parse_check_response(input).unwrap();
        assert!(r.answered);
    }

    #[test]
    fn parse_no_verdict() {
        let input = "```verdict\nNO\n```\n```reason\nOnly HTML headers were returned, no actual content.\n```";
        let r = parse_check_response(input).unwrap();
        assert!(!r.answered);
        assert!(r.reason.contains("HTML headers"));
    }

    #[test]
    fn parse_missing_blocks_defaults_to_no() {
        let r = parse_check_response("some random text").unwrap();
        assert!(!r.answered);
        assert_eq!(r.reason, "unknown");
    }

    #[test]
    fn parse_case_insensitive() {
        let input = "```verdict\nyes\n```\n```reason\nall good\n```";
        let r = parse_check_response(input).unwrap();
        assert!(r.answered);
    }
}
