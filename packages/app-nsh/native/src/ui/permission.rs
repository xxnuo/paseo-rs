use super::Conversation;
use crate::{
    protocol::conversation::{
        PermissionBehavior, PermissionKind, PermissionRequest, PermissionResponse,
    },
    state::conversation::{PendingPermission, PermissionStatus},
};
use nsh_base::text_view::{Block, Document, Inline};
use nsh_base::{button, checkbox, input, text_view, theme};
use nsh_core::prelude::*;
use serde_json::{Map, Value};
use std::rc::Rc;

struct QuestionOption {
    label: String,
    description: Option<String>,
    preview: Option<String>,
}

struct Question {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
    allow_other: bool,
    allow_empty: bool,
    placeholder: Option<String>,
    dismiss_label: Option<String>,
}

#[derive(Clone, Default)]
struct Answer {
    // Preserve selection order, as the original question form's Set does.
    selected: Vec<usize>,
    other: String,
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn parse_questions(input: Option<&Map<String, Value>>) -> Option<Vec<Question>> {
    let raw = input?.get("questions")?.as_array()?;
    if raw.is_empty() {
        return None;
    }
    raw.iter()
        .map(|value| {
            Some(Question {
                question: value.get("question")?.as_str()?.into(),
                header: value.get("header")?.as_str()?.into(),
                options: value
                    .get("options")?
                    .as_array()?
                    .iter()
                    .map(|option| {
                        Some(QuestionOption {
                            label: option.get("label")?.as_str()?.into(),
                            description: optional_string(option, "description"),
                            preview: optional_string(option, "preview"),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                multi_select: value.get("multiSelect") == Some(&Value::Bool(true)),
                allow_other: value.get("allowOther") == Some(&Value::Bool(true))
                    || value.get("isOther") == Some(&Value::Bool(true)),
                allow_empty: value.get("allowEmpty") == Some(&Value::Bool(true)),
                placeholder: optional_string(value, "placeholder"),
                dismiss_label: optional_string(value, "dismissLabel"),
            })
        })
        .collect()
}

impl Question {
    fn shows_input(&self) -> bool {
        self.options.is_empty() || self.allow_other
    }

    fn answered(&self, answer: &Answer) -> bool {
        answer
            .selected
            .iter()
            .any(|&index| index < self.options.len())
            || (self.shows_input() && (!answer.other.trim().is_empty() || self.allow_empty))
    }

    fn toggle(&self, answer: &mut Answer, index: usize) {
        if index >= self.options.len() {
            return;
        }
        let selected = answer.selected.contains(&index);
        if !self.multi_select {
            answer.selected.clear();
            answer.other.clear();
        }
        if selected {
            answer.selected.retain(|&selected| selected != index);
        } else {
            answer.selected.push(index);
        }
    }

    fn set_other(&self, answer: &mut Answer, text: &str) {
        answer.other = text.into();
        if !self.multi_select && !text.is_empty() {
            answer.selected.clear();
        }
    }
}

fn build_answers(questions: &[Question], answers: &[Answer]) -> Map<String, Value> {
    let mut result = Map::new();
    for (question, answer) in questions.iter().zip(answers) {
        let mut labels: Vec<_> = answer
            .selected
            .iter()
            .filter_map(|&index| {
                question
                    .options
                    .get(index)
                    .map(|option| option.label.clone())
            })
            .collect();
        let other = answer.other.trim();
        if question.shows_input() && !other.is_empty() {
            if !question.multi_select {
                labels.clear();
            }
            labels.push(other.into());
        } else if question.shows_input() && question.allow_empty && question.options.is_empty() {
            result.insert(question.header.clone(), Value::String(String::new()));
            continue;
        }
        if !labels.is_empty() {
            result.insert(question.header.clone(), Value::String(labels.join(", ")));
        }
    }
    result
}

fn question_response(
    request: &PermissionRequest,
    questions: &[Question],
    answers: &[Answer],
) -> Option<PermissionResponse> {
    if questions.is_empty()
        || questions.len() != answers.len()
        || !questions
            .iter()
            .zip(answers)
            .all(|(question, answer)| question.answered(answer))
    {
        return None;
    }
    let mut updated_input = request.input.clone()?;
    updated_input.insert(
        "answers".into(),
        Value::Object(build_answers(questions, answers)),
    );
    Some(PermissionResponse::Allow {
        selected_action_id: None,
        updated_input: Some(updated_input),
        updated_permissions: None,
    })
}

fn deny() -> PermissionResponse {
    PermissionResponse::Deny {
        selected_action_id: None,
        message: Some("Denied by user".into()),
        interrupt: None,
    }
}

fn details(request: &PermissionRequest) -> String {
    let mut lines = Vec::new();
    if request.kind == PermissionKind::Plan {
        if let Some(plan) = request
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("planText"))
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .input
                    .as_ref()
                    .and_then(|input| input.get("plan"))
                    .and_then(Value::as_str)
            })
        {
            lines.push(plan.to_owned());
        }
    }
    for key in ["command", "filePath", "path", "url", "query", "text"] {
        let value = request
            .detail
            .as_ref()
            .and_then(|detail| detail.fields.get(key))
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .input
                    .as_ref()
                    .and_then(|input| input.get(key))
                    .and_then(Value::as_str)
            });
        if let Some(value) = value {
            lines.push(value.to_owned());
        }
    }
    lines.join("\n\n")
}

pub(super) fn card(
    state: Rc<Conversation>,
    pending: ReadSignal<PendingPermission>,
) -> impl IntoElement {
    let question =
        pending.with_untracked(|pending| pending.request.kind == PermissionKind::Question);
    let content = if question {
        questions(state, pending).into_element()
    } else {
        actions(state, pending).into_element()
    };
    view()
        .p_3()
        .gap_2()
        .border_1()
        .border_color(move || theme().input)
        .child(
            text(move || {
                pending.with(|pending| {
                    pending
                        .request
                        .title
                        .clone()
                        .unwrap_or_else(|| pending.request.name.clone())
                })
            })
            .text_color(move || theme().foreground),
        )
        .child(
            text(move || {
                pending.with(|pending| pending.request.description.clone().unwrap_or_default())
            })
            .text_color(move || theme().muted_foreground),
        )
        .child(
            text_view()
                .markdown(move || {
                    pending.with(|pending| Some(SharedString::from(details(&pending.request))))
                })
                .selectable(true)
                .text_color(move || theme().foreground),
        )
        .child(content)
        .child(
            text(move || {
                pending.with(|pending| match pending.status {
                    PermissionStatus::Submitting => "正在等待主机确认…".to_owned(),
                    PermissionStatus::Failed => format!(
                        "处理失败：{}。可保留当前输入重试",
                        pending.error.as_deref().unwrap_or("尚未确认处理结果")
                    ),
                    PermissionStatus::Pending => "等待你的选择".to_owned(),
                })
            })
            .text_color(move || theme().muted_foreground),
        )
}

fn actions(state: Rc<Conversation>, pending: ReadSignal<PendingPermission>) -> impl IntoElement {
    let count =
        pending.with_untracked(|pending| pending.request.actions.as_ref().map_or(0, Vec::len));
    let mut controls = view().gap_2();
    for index in 0..count {
        let state = state.clone();
        controls = controls.child(
            button()
                .disabled(move || {
                    pending.with(|pending| pending.status == PermissionStatus::Submitting)
                })
                .on_press(move || {
                    let choice = pending.with_untracked(|pending| {
                        let action = pending.request.actions.as_ref()?.get(index)?;
                        let response = match action.behavior {
                            PermissionBehavior::Allow => PermissionResponse::Allow {
                                selected_action_id: Some(action.id.clone()),
                                updated_input: None,
                                updated_permissions: None,
                            },
                            PermissionBehavior::Deny => PermissionResponse::Deny {
                                selected_action_id: Some(action.id.clone()),
                                message: Some("Denied by user".into()),
                                interrupt: None,
                            },
                        };
                        Some((pending.request.id.clone(), response))
                    });
                    if let Some((id, response)) = choice {
                        state.respond(id, response);
                    }
                })
                .p_2()
                .child(text(move || {
                    pending.with(|pending| {
                        pending
                            .request
                            .actions
                            .as_ref()
                            .and_then(|actions| actions.get(index))
                            .map(|action| action.label.clone())
                            .unwrap_or_default()
                    })
                })),
        );
    }
    if count == 0 {
        let reject = state.clone();
        controls = controls
            .child(
                button()
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_press(move || {
                        reject.respond(
                            pending.with_untracked(|pending| pending.request.id.clone()),
                            deny(),
                        )
                    })
                    .p_2()
                    .child(text("拒绝")),
            )
            .child(
                button()
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_press(move || {
                        state.respond(
                            pending.with_untracked(|pending| pending.request.id.clone()),
                            PermissionResponse::Allow {
                                selected_action_id: None,
                                updated_input: None,
                                updated_permissions: None,
                            },
                        )
                    })
                    .p_2()
                    .child(text("允许")),
            );
    }
    controls
}

fn questions(state: Rc<Conversation>, pending: ReadSignal<PendingPermission>) -> impl IntoElement {
    let parsed = pending.with_untracked(|pending| parse_questions(pending.request.input.as_ref()));
    let Some(parsed) = parsed else {
        return view()
            .gap_2()
            .child(text("暂不支持此问题格式，无法提交答案；可拒绝此请求"))
            .child(
                button()
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_press(move || {
                        state.respond(
                            pending.with_untracked(|pending| pending.request.id.clone()),
                            deny(),
                        )
                    })
                    .p_2()
                    .child(text("拒绝")),
            )
            .child(button().disabled(true).p_2().child(text("提交答案")))
            .into_element();
    };
    let questions = Rc::new(parsed);
    let answers = signal(vec![Answer::default(); questions.len()]);
    let mut form = view().gap_2();
    for (question_index, question) in questions.iter().enumerate() {
        let mut block = view().gap_2().child(
            text(format!("{} · {}", question.header, question.question))
                .text_color(move || theme().foreground),
        );
        let mut options = view()
            .gap_2()
            .role(if question.multi_select {
                AccessibilityRole::None
            } else {
                AccessibilityRole::Radiogroup
            })
            .label(question.question.clone());
        for (option_index, option) in question.options.iter().enumerate() {
            let models = questions.clone();
            let label = option.label.clone();
            let description = option.description.clone().unwrap_or_default();
            let preview = option.preview.clone().unwrap_or_default();
            options = options.child(
                checkbox()
                    .role(if question.multi_select {
                        AccessibilityRole::Checkbox
                    } else {
                        AccessibilityRole::Radio
                    })
                    .accessibility_label(option.label.clone())
                    .checked(move || {
                        answers.with(|answers| {
                            answers[question_index].selected.contains(&option_index)
                        })
                    })
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_change(move |_: &bool| {
                        answers.update(|answers| {
                            models[question_index]
                                .toggle(&mut answers[question_index], option_index)
                        })
                    })
                    .indicator(
                        text(move || {
                            if answers.with(|answers| {
                                answers[question_index].selected.contains(&option_index)
                            }) {
                                "[x]"
                            } else {
                                "[ ]"
                            }
                        })
                        .text_color(move || theme().foreground),
                    )
                    .flex_row()
                    .gap_2()
                    .p_2()
                    .child(
                        view()
                            .gap_2()
                            .child(text(label).text_color(move || theme().foreground))
                            .child(text(description).text_color(move || theme().muted_foreground))
                            .child(
                                text_view()
                                    .document(Some(Document::new(vec![Block::paragraph(vec![
                                        Inline::text(preview),
                                    ])])))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    ),
            );
        }
        block = block.child(options);
        if question.shows_input() {
            let models = questions.clone();
            let placeholder = question.placeholder.clone().unwrap_or_else(|| {
                if question.options.is_empty() {
                    "输入答案"
                } else {
                    "输入其他答案"
                }
                .into()
            });
            block = block.child(
                input()
                    .label(question.question.clone())
                    .placeholder(placeholder)
                    .value(move || answers.with(|answers| answers[question_index].other.clone()))
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_change(move |value: &str| {
                        answers.update(|answers| {
                            models[question_index].set_other(&mut answers[question_index], value)
                        })
                    })
                    .p_3()
                    .border_1()
                    .border_color(move || theme().input)
                    .text_color(move || theme().foreground),
            );
        }
        form = form.child(block);
    }
    let dismiss_label = questions
        .iter()
        .find_map(|question| question.dismiss_label.clone())
        .unwrap_or_else(|| "拒绝".into());
    let dismiss_models = questions.clone();
    let disabled_models = questions.clone();
    let submit_models = questions.clone();
    let dismiss = state.clone();
    form.child(
        view()
            .gap_2()
            .child(
                button()
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                    })
                    .on_press(move || {
                        let id = pending.with_untracked(|pending| pending.request.id.clone());
                        let empty_on_dismiss = dismiss_models
                            .iter()
                            .all(|question| question.allow_empty && question.options.is_empty());
                        let response = if empty_on_dismiss {
                            pending.with_untracked(|pending| {
                                answers.with_untracked(|answers| {
                                    question_response(&pending.request, &dismiss_models, answers)
                                })
                            })
                        } else {
                            Some(deny())
                        };
                        if let Some(response) = response {
                            dismiss.respond(id, response);
                        }
                    })
                    .p_2()
                    .child(text(dismiss_label)),
            )
            .child(
                button()
                    .disabled(move || {
                        pending.with(|pending| pending.status == PermissionStatus::Submitting)
                            || !answers.with(|answers| {
                                disabled_models
                                    .iter()
                                    .zip(answers)
                                    .all(|(question, answer)| question.answered(answer))
                            })
                    })
                    .on_press(move || {
                        let id = pending.with_untracked(|pending| pending.request.id.clone());
                        let response = pending.with_untracked(|pending| {
                            answers.with_untracked(|answers| {
                                question_response(&pending.request, &submit_models, answers)
                            })
                        });
                        if let Some(response) = response {
                            state.respond(id, response);
                        }
                    })
                    .p_2()
                    .child(text("提交答案")),
            ),
    )
    .into_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn questions(value: Value) -> Vec<Question> {
        parse_questions(value.as_object()).unwrap()
    }

    #[test]
    fn question_validation_matches_required_and_optional_upstream_fields() {
        for input in [
            Value::Null,
            json!({}),
            json!({"questions":[]}),
            json!({"questions":[{"question":"Q", "header":"H", "options":[{"label":42}]}]}),
        ] {
            assert!(parse_questions(input.as_object()).is_none());
        }
        let parsed = questions(
            json!({"questions":[{"question":"Q", "header":"H", "options":[{"label":"A", "description":"D", "preview":"P"}], "isOther":true, "multiSelect":"true", "placeholder":7}]}),
        );
        assert!(parsed[0].shows_input());
        assert!(!parsed[0].multi_select);
        assert!(parsed[0].placeholder.is_none());
        assert_eq!(parsed[0].options[0].description.as_deref(), Some("D"));
        assert_eq!(parsed[0].options[0].preview.as_deref(), Some("P"));
    }

    #[test]
    fn single_select_and_custom_answer_replace_each_other_in_controlled_input() {
        let parsed = questions(
            json!({"questions":[{"question":"Q", "header":"Provider", "options":[{"label":"A"},{"label":"B"}], "allowOther":true}]}),
        );
        let mut answer = Answer::default();
        assert!(!parsed[0].answered(&answer));
        parsed[0].toggle(&mut answer, 1);
        assert_eq!(build_answers(&parsed, &[answer.clone()])["Provider"], "B");
        parsed[0].set_other(&mut answer, " custom ");
        assert!(answer.selected.is_empty());
        assert_eq!(
            build_answers(&parsed, &[answer.clone()])["Provider"],
            "custom"
        );
        parsed[0].toggle(&mut answer, 0);
        assert!(answer.other.is_empty());
        parsed[0].toggle(&mut answer, 0);
        assert!(!parsed[0].answered(&answer));
    }

    #[test]
    fn multi_select_preserves_selection_order_and_appends_other_answer() {
        let parsed = questions(
            json!({"questions":[{"question":"Q", "header":"Fruits", "options":[{"label":"Apple"},{"label":"Banana"},{"label":"Cherry"}], "multiSelect":true, "allowOther":true}]}),
        );
        let mut answer = Answer::default();
        parsed[0].toggle(&mut answer, 2);
        parsed[0].toggle(&mut answer, 0);
        parsed[0].set_other(&mut answer, " durian ");
        assert_eq!(
            build_answers(&parsed, &[answer.clone()])["Fruits"],
            "Cherry, Apple, durian"
        );
        parsed[0].toggle(&mut answer, 2);
        assert_eq!(build_answers(&parsed, &[answer])["Fruits"], "Apple, durian");
    }

    #[test]
    fn allow_empty_only_answers_text_questions_and_does_not_bypass_option_choice() {
        let parsed = questions(
            json!({"questions":[{"question":"Optional?", "header":"Response", "options":[], "allowEmpty":true, "dismissLabel":"Skip"}]}),
        );
        assert!(parsed[0].answered(&Answer::default()));
        assert_eq!(build_answers(&parsed, &[Answer::default()])["Response"], "");
        assert_eq!(parsed[0].dismiss_label.as_deref(), Some("Skip"));
        let option_only = questions(
            json!({"questions":[{"question":"Q", "header":"H", "options":[{"label":"A"}], "allowEmpty":true}]}),
        );
        assert!(!option_only[0].answered(&Answer::default()));
        assert!(!option_only[0].answered(&Answer {
            other: "ignored".into(),
            ..Default::default()
        }));
    }

    #[test]
    fn submit_requires_all_answers_and_preserves_original_provider_input() {
        let request: PermissionRequest = serde_json::from_value(json!({"id":"p", "provider":"test", "name":"ask", "kind":"question", "input":{
            "providerField":{"keep":true}, "questions":[{"question":"Q", "header":"Answer", "options":[]}]}})).unwrap();
        let parsed = parse_questions(request.input.as_ref()).unwrap();
        assert!(question_response(&request, &parsed, &[Answer::default()]).is_none());
        assert!(question_response(&request, &parsed, &[]).is_none());
        let response = question_response(
            &request,
            &parsed,
            &[Answer {
                other: " yes ".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let PermissionResponse::Allow {
            selected_action_id,
            updated_input: Some(input),
            ..
        } = response
        else {
            panic!("expected answers");
        };
        assert!(selected_action_id.is_none());
        assert_eq!(input["providerField"]["keep"], true);
        assert_eq!(input["answers"], json!({"Answer":"yes"}));
        assert_eq!(
            input["questions"],
            request.input.as_ref().unwrap()["questions"]
        );
    }
}
