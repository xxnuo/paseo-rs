//! Read-only tool details for Markdown rendering; tool status and private metadata stay untouched.

use crate::protocol::conversation::{ToolCall, ToolStatus};
use serde_json::{Map, Value};

pub(crate) struct ToolPresentation {
    pub(crate) markdown: String,
    pub(crate) loading: bool,
}

#[derive(Clone, Copy)]
enum Format {
    Markdown,
    Code(&'static str),
}

/// Project only the detail's documented display fields, never ToolCall metadata or extensions.
pub(crate) fn present_tool(tool: &ToolCall) -> ToolPresentation {
    use Format::{Code, Markdown};
    let fields = &tool.detail.fields;
    let display: &[(&str, &str, Format)] = match tool.detail.kind.as_str() {
        "shell" => &[
            ("command", "命令", Code("")),
            ("cwd", "工作目录", Code("")),
            ("output", "输出", Code("")),
            ("exitCode", "退出码", Code("")),
        ],
        "read" => &[
            ("filePath", "文件", Code("")),
            ("offset", "起始行", Code("")),
            ("limit", "读取行数", Code("")),
            ("content", "内容", Code("")),
        ],
        "write" => &[
            ("filePath", "文件", Code("")),
            ("content", "内容", Code("")),
        ],
        "edit" => &[
            ("filePath", "文件", Code("")),
            ("unifiedDiff", "差异", Code("diff")),
            ("oldString", "修改前", Code("")),
            ("newString", "修改后", Code("")),
        ],
        "search" => &[
            ("query", "查询", Code("")),
            ("toolName", "搜索方式", Code("")),
            ("mode", "结果类型", Code("")),
            ("content", "结果", Code("")),
            ("filePaths", "文件", Code("")),
            ("webResults", "网页结果", Code("")),
            ("annotations", "说明", Code("")),
            ("numFiles", "文件数", Code("")),
            ("numMatches", "匹配数", Code("")),
            ("durationMs", "耗时（毫秒）", Code("")),
            ("durationSeconds", "耗时（秒）", Code("")),
            ("truncated", "结果已截断", Code("")),
        ],
        "fetch" => &[
            ("url", "网址", Code("")),
            ("prompt", "请求", Code("")),
            ("result", "结果", Code("")),
            ("code", "响应码", Code("")),
            ("codeText", "响应状态", Code("")),
            ("bytes", "字节数", Code("")),
            ("durationMs", "耗时（毫秒）", Code("")),
        ],
        "worktree_setup" => &[
            ("worktreePath", "工作区目录", Code("")),
            ("branchName", "分支", Code("")),
            ("log", "日志", Code("")),
            ("commands", "执行记录", Code("")),
            ("truncated", "日志已截断", Code("")),
        ],
        "sub_agent" => &[
            ("subAgentType", "子 Agent 类型", Code("")),
            ("description", "说明", Code("")),
            ("childSessionId", "子 Agent 会话", Code("")),
            ("log", "日志", Code("")),
            ("actions", "操作记录", Code("")),
        ],
        "plain_text" => &[("label", "说明", Code("")), ("text", "内容", Markdown)],
        "plan" => &[("text", "计划", Markdown)],
        "unknown" => &[("input", "输入", Code("")), ("output", "输出", Code(""))],
        // The current protocol rejects unknown kinds; keep this projection readable if it expands.
        _ => &[
            ("input", "输入", Code("")),
            ("output", "输出", Code("")),
            ("text", "内容", Code("")),
        ],
    };
    let mut sections = Vec::new();
    for &(key, title, format) in display {
        let Some(value) = fields.get(key) else {
            continue;
        };
        let projected;
        let value = match (tool.detail.kind.as_str(), key) {
            ("search", "webResults") => {
                projected = record_fields(value, &["title", "url"]);
                &projected
            }
            ("worktree_setup", "commands") => {
                projected = record_fields(
                    value,
                    &[
                        "index",
                        "command",
                        "cwd",
                        "log",
                        "status",
                        "exitCode",
                        "durationMs",
                    ],
                );
                &projected
            }
            ("sub_agent", "actions") => {
                projected = record_fields(value, &["index", "toolName", "summary"]);
                &projected
            }
            _ => value,
        };
        if meaningful(value) {
            sections.push(section(title, value, format));
        }
    }
    // Errors are an independent result: even an empty object/string must not disappear.
    if !tool.error.is_null() {
        sections.push(section("错误", &tool.error, Code("")));
    }
    let loading = sections.is_empty() && tool.status == ToolStatus::Running;
    let markdown = if sections.is_empty() {
        match tool.status {
            ToolStatus::Running => "正在等待工具详情…",
            ToolStatus::Completed => "工具已完成，未返回详细输出。",
            ToolStatus::Failed => "工具失败，未返回详细输出。",
            ToolStatus::Canceled => "工具已取消，未返回详细输出。",
        }
        .to_owned()
    } else {
        sections.join("\n\n")
    };
    ToolPresentation { markdown, loading }
}

fn record_fields(value: &Value, keys: &[&str]) -> Value {
    let Value::Array(rows) = value else {
        return value.clone();
    };
    Value::Array(
        rows.iter()
            .map(|row| match row {
                Value::Object(fields) => Value::Object(
                    keys.iter()
                        .filter_map(|&key| {
                            fields.get(key).map(|value| (key.to_owned(), value.clone()))
                        })
                        .collect::<Map<_, _>>(),
                ),
                _ => row.clone(),
            })
            .collect(),
    )
}

fn meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(values) => values.iter().any(meaningful),
        Value::Object(fields) => fields.values().any(meaningful),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn section(title: &str, value: &Value, format: Format) -> String {
    let (text, language) = match value {
        Value::String(text) => (text.clone(), ""),
        _ => (
            serde_json::to_string_pretty(value).expect("a JSON Value is serializable"),
            "json",
        ),
    };
    let body = match format {
        Format::Markdown if value.is_string() => text,
        Format::Markdown => code_block(&text, language),
        Format::Code(hint) => code_block(&text, if language.is_empty() { hint } else { language }),
    };
    format!("### {title}\n\n{body}")
}

fn code_block(text: &str, language: &str) -> String {
    // A tool's literal output cannot close its fence and become Markdown links or instructions.
    let mut longest = 0;
    let mut run = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}{language}\n{text}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(status: &str, detail: Value, error: Value) -> ToolCall {
        serde_json::from_value(json!({
            "callId":"presentation-call", "name":"fixture", "status":status, "detail":detail,
            "error":error, "metadata":{"credential":"DO_NOT_RENDER"},
            "privateExtension":"DO_NOT_RENDER"
        }))
        .unwrap()
    }

    #[test]
    fn real_unknown_tool_capture_keeps_structured_input_and_output_visible() {
        let capture: Vec<Value> =
            serde_json::from_str(include_str!("../../../protocol-fixtures/timeline.json")).unwrap();
        let item = capture
            .iter()
            .find(|row| row["frame"]["message"]["type"] == "fetch_agent_timeline_response")
            .unwrap()["frame"]["message"]["payload"]["entries"][1]["item"]
            .clone();
        let tool: ToolCall = serde_json::from_value(item).unwrap();
        let result = present_tool(&tool);
        assert!(!result.loading);
        assert!(result.markdown.contains("### 输入\n\n```json\n"));
        assert!(result.markdown.contains("\"command\": \"echo hello\""));
        assert!(result.markdown.contains("### 输出\n\n```json\n"));
        assert!(result.markdown.contains("\"stdout\": \"hello\\n\""));
    }

    #[test]
    fn structured_errors_and_false_or_zero_results_are_not_dropped() {
        let failed = tool(
            "failed",
            json!({"type":"unknown","input":{"enabled":false},"output":0}),
            json!({"message":"permission denied","code":403,"context":["scope","path"]}),
        );
        let result = present_tool(&failed);
        for text in [
            "false",
            "### 输出",
            "\n0\n",
            "### 错误",
            "permission denied",
            "403",
            "scope",
        ] {
            assert!(result.markdown.contains(text), "missing {text}");
        }
        assert!(!result.loading);
        assert!(!result.markdown.contains("DO_NOT_RENDER"));
        let array_error = tool(
            "failed",
            json!({"type":"unknown","input":null,"output":["first",{"value":false}]}),
            json!(["stderr",{"code":2}]),
        );
        let result = present_tool(&array_error);
        for text in ["first", "false", "### 错误", "stderr", "\"code\": 2"] {
            assert!(result.markdown.contains(text), "missing {text}");
        }
    }

    #[test]
    fn all_typed_detail_variants_keep_their_display_content() {
        let cases = [
            (
                json!({"type":"shell","command":"printf hello","cwd":"/work","output":"stdout\nstderr","exitCode":7}),
                vec!["printf hello", "/work", "stdout\nstderr", "退出码", "7"],
            ),
            (
                json!({"type":"read","filePath":"file.rs","content":"fn main() {}","offset":3,"limit":9}),
                vec!["file.rs", "fn main() {}", "起始行", "读取行数"],
            ),
            (
                json!({"type":"write","filePath":"new.txt","content":"write body"}),
                vec!["new.txt", "write body"],
            ),
            (
                json!({"type":"edit","filePath":"file.rs","unifiedDiff":"-old\n+new","oldString":"before","newString":"after"}),
                vec!["file.rs", "```diff", "-old\n+new", "before", "after"],
            ),
            (
                json!({"type":"search","query":"needle","content":"match body","filePaths":["one.rs"],"webResults":[{"title":"Page","url":"https://example.test","metadata":"DO_NOT_RENDER"}],"annotations":["Search incomplete"],"numFiles":1,"numMatches":2,"durationMs":5,"durationSeconds":0.005,"truncated":true,"mode":"content","toolName":"grep"}),
                vec![
                    "needle",
                    "match body",
                    "one.rs",
                    "Page",
                    "https://example.test",
                    "Search incomplete",
                    "文件数",
                    "匹配数",
                    "耗时（毫秒）",
                    "耗时（秒）",
                    "结果已截断",
                    "grep",
                ],
            ),
            (
                json!({"type":"fetch","url":"https://example.test","prompt":"extract title","result":"fetched body","code":200,"codeText":"OK","bytes":32,"durationMs":4}),
                vec![
                    "https://example.test",
                    "extract title",
                    "fetched body",
                    "200",
                    "OK",
                    "32",
                ],
            ),
            (
                json!({"type":"worktree_setup","worktreePath":"/worktree","branchName":"feature","log":"setup log","commands":[{"index":0,"command":"install","cwd":"/worktree","log":"command log","status":"failed","exitCode":1,"durationMs":12,"metadata":"DO_NOT_RENDER"}],"truncated":false}),
                vec![
                    "/worktree",
                    "feature",
                    "setup log",
                    "install",
                    "command log",
                    "failed",
                    "exitCode",
                ],
            ),
            (
                json!({"type":"sub_agent","subAgentType":"reviewer","description":"review task","childSessionId":"child-session","log":"child log","actions":[{"index":0,"toolName":"read","summary":"inspected source","private":"DO_NOT_RENDER"}]}),
                vec![
                    "reviewer",
                    "review task",
                    "child-session",
                    "child log",
                    "inspected source",
                ],
            ),
            (
                json!({"type":"plain_text","label":"Summary","text":"**keep this markdown**"}),
                vec!["Summary", "**keep this markdown**"],
            ),
            (
                json!({"type":"plan","text":"1. inspect\n2. fix"}),
                vec!["1. inspect\n2. fix"],
            ),
        ];
        for (detail, expected) in cases {
            let result = present_tool(&tool("completed", detail, Value::Null));
            assert!(!result.loading);
            for text in expected {
                assert!(result.markdown.contains(text), "missing {text}");
            }
            assert!(!result.markdown.contains("DO_NOT_RENDER"));
        }
    }

    #[test]
    fn loading_and_empty_results_follow_the_actual_status_without_mutating_it() {
        for (status, expected, loading) in [
            ("running", "正在等待工具详情…", true),
            ("completed", "工具已完成，未返回详细输出。", false),
            ("canceled", "工具已取消，未返回详细输出。", false),
        ] {
            let tool = tool(
                status,
                json!({"type":"unknown","input":{},"output":null}),
                Value::Null,
            );
            let result = present_tool(&tool);
            assert_eq!(result.markdown, expected);
            assert_eq!(result.loading, loading);
            assert_eq!(tool.status == ToolStatus::Running, status == "running");
        }
        let failed = tool(
            "failed",
            json!({"type":"unknown","input":null,"output":null}),
            json!({}),
        );
        assert!(
            present_tool(&failed)
                .markdown
                .contains("### 错误\n\n```json\n{}\n```")
        );
        assert!(!present_tool(&failed).loading);
        let empty = tool(
            "running",
            json!({"type":"unknown","input":{"unused":[null,"  ",{}]},"output":null}),
            Value::Null,
        );
        assert!(present_tool(&empty).loading);
    }

    #[test]
    fn literal_outputs_cannot_escape_their_fences_and_prose_keeps_original_markdown() {
        let output = "\n````\n[do not execute](javascript:alert(1))\n~~~\n$ not an added prompt\n";
        let result = present_tool(&tool(
            "completed",
            json!({"type":"shell","command":"read only","output":output}),
            Value::Null,
        ));
        assert!(result.markdown.contains(&format!("`````\n{output}\n`````")));
        assert!(!result.markdown.contains("$ read only"));
        let prose = "# Original title\n\n**bold**\n\n- item\n";
        let result = present_tool(&tool(
            "completed",
            json!({"type":"plan","text":prose}),
            Value::Null,
        ));
        assert_eq!(result.markdown, format!("### 计划\n\n{prose}"));
    }

    #[test]
    fn large_outputs_are_complete_and_only_documented_fields_are_projected() {
        let output = format!("{}END_OF_OUTPUT", "line\n".repeat(20_000));
        let result = present_tool(&tool(
            "completed",
            json!({"type":"unknown","input":null,"output":output,"metadata":{"private":"DO_NOT_RENDER"}}),
            Value::Null,
        ));
        assert!(result.markdown.contains(&output));
        assert!(!result.markdown.contains("DO_NOT_RENDER"));
        let mut future = tool(
            "completed",
            json!({"type":"unknown","input":{"path":"visible"},"output":null,"private":"DO_NOT_RENDER"}),
            Value::Null,
        );
        future.detail.kind = "future_kind".into();
        let result = present_tool(&future);
        assert!(result.markdown.contains("visible"));
        assert!(!result.markdown.contains("DO_NOT_RENDER"));
    }
}
