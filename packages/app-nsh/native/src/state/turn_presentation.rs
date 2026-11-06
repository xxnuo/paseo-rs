//! Turn footer placement and timing; never changes timeline rows or tool outcomes.

use super::{conversation::Turn, timeline::Row};
use crate::protocol::conversation::TimelineItem;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timestamp {
    pub(crate) unix_ms: i64,
}

impl Timestamp {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| Self {
                unix_ms: date.timestamp_millis(),
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowKey {
    pub(crate) epoch: String,
    pub(crate) id: u64,
}

impl From<&Row> for RowKey {
    fn from(row: &Row) -> Self {
        Self {
            epoch: row.epoch.clone(),
            id: row.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Timing {
    pub(crate) completed_at: Timestamp,
    pub(crate) completed_at_raw: String,
    pub(crate) duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedFooter {
    /// Render after this chronological row, even when an earlier assistant owns the actions.
    pub(crate) after: RowKey,
    pub(crate) host: RowKey,
    /// Full turn membership; a copy action can collect only assistant text in this range.
    pub(crate) response_range: Range<usize>,
    pub(crate) timing: Option<Timing>,
    pub(crate) is_latest: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunningFooter {
    pub(crate) started_at: Option<Timestamp>,
    pub(crate) elapsed_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnPresentation {
    pub(crate) completed: Vec<CompletedFooter>,
    /// Render once at the timeline bottom, including while awaiting the first response row.
    pub(crate) running: Option<RunningFooter>,
}

/// Input rows are chronological loaded timeline rows, excluding optimistic pending messages.
/// `has_active_submission` means a provider-unacknowledged submission, as in the original app;
/// an accepted RPC receipt alone does not prove a running turn.
pub(crate) fn project(
    rows: &[Row],
    current: Option<&Turn>,
    has_active_submission: bool,
    now_ms: i64,
) -> TurnPresentation {
    let active = current.is_some() || has_active_submission;
    let mut result = TurnPresentation {
        completed: Vec::new(),
        running: active.then(|| {
            let started_at = current
                .and_then(|turn| turn.started_at.as_deref())
                .and_then(Timestamp::parse);
            RunningFooter {
                started_at,
                elapsed_ms: started_at.map(|start| elapsed(now_ms, start.unix_ms)),
            }
        }),
    };
    let mut start = 0;
    for end in 1..=rows.len() {
        let boundary = end == rows.len() || !continues_turn(&rows[end - 1], &rows[end]);
        if boundary {
            if end != rows.len() || !active {
                if let Some(footer) = completed_footer(rows, start..end) {
                    result.completed.push(footer);
                }
            }
            start = end;
        }
    }
    result
}

fn elapsed(end: i64, start: i64) -> u64 {
    end.saturating_sub(start).max(0) as u64
}

fn trigger(row: &Row) -> bool {
    match &*row.item {
        TimelineItem::User(_) => true,
        TimelineItem::Tool(tool) => {
            tool.name == "heartbeat" || tool.extra.contains_key("agentMessage")
        }
        _ => false,
    }
}

fn continues_turn(previous: &Row, next: &Row) -> bool {
    if previous.epoch != next.epoch {
        return false;
    }
    match (&previous.turn_id, &next.turn_id) {
        (Some(previous), Some(next)) => previous == next,
        _ => !trigger(next),
    }
}

fn completed_footer(rows: &[Row], range: Range<usize>) -> Option<CompletedFooter> {
    let last = rows.get(range.end.checked_sub(1)?)?;
    let host = rows[range.clone()]
        .iter()
        .rev()
        .find(|row| matches!(&*row.item, TimelineItem::Assistant(_)))
        .or_else(|| matches!(&*last.item, TimelineItem::Tool(_)).then_some(last))?;
    let timing = Timestamp::parse(&last.timestamp).map(|completed_at| {
        let first = &rows[range.start];
        let started_at = trigger(first)
            .then(|| Timestamp::parse(&first.timestamp))
            .flatten();
        Timing {
            completed_at,
            completed_at_raw: last.timestamp.clone(),
            duration_ms: started_at.map(|start| elapsed(completed_at.unix_ms, start.unix_ms)),
        }
    });
    Some(CompletedFooter {
        after: last.into(),
        host: host.into(),
        is_latest: range.end == rows.len(),
        response_range: range,
        timing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::timeline::SourceRange;
    use serde_json::{Value, json};
    use std::rc::Rc;

    fn row(id: u64, second: u64, turn: Option<&str>, item: Value) -> Row {
        Row {
            id,
            epoch: "epoch".into(),
            provider: "mock".into(),
            turn_id: turn.map(str::to_owned),
            timestamp: format!("2026-11-06T00:00:{second:02}.000Z"),
            span: SourceRange { start: id, end: id },
            sources: vec![SourceRange { start: id, end: id }],
            collapsed: Rc::new(vec![]),
            item: Rc::new(serde_json::from_value(item).unwrap()),
            provisional: false,
            active: false,
        }
    }
    fn user(id: u64, second: u64, turn: Option<&str>) -> Row {
        row(
            id,
            second,
            turn,
            json!({"type":"user_message","text":"prompt"}),
        )
    }
    fn assistant(id: u64, second: u64, turn: Option<&str>) -> Row {
        row(
            id,
            second,
            turn,
            json!({"type":"assistant_message","text":"reply"}),
        )
    }
    fn tool(id: u64, second: u64, turn: Option<&str>, name: &str) -> Row {
        row(
            id,
            second,
            turn,
            json!({"type":"tool_call","callId":format!("call-{id}"),"name":name,"status":"running","detail":{"type":"unknown","input":{},"output":null},"error":null}),
        )
    }
    fn millis(second: i64) -> i64 {
        Timestamp::parse("2026-11-06T00:00:00Z").unwrap().unix_ms + second * 1000
    }

    #[test]
    fn timestamp_offsets_fractional_seconds_and_calendar_boundaries_are_validated() {
        for (value, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1970-01-01T08:00:00+08:00", 0),
            ("1969-12-31T23:59:59.999Z", -1),
            ("1970-01-01T00:00:00.1Z", 100),
            ("1970-01-01T00:00:00.123456Z", 123),
            ("1969-12-31T23:00:00-01:00", 0),
        ] {
            assert_eq!(
                Timestamp::parse(value),
                Some(Timestamp { unix_ms: expected }),
                "{value}"
            );
        }
        assert!(Timestamp::parse("2000-02-29T00:00:00Z").is_some());
        assert!(Timestamp::parse("2024-02-29T00:00:00Z").is_some());
        for value in [
            "now",
            "",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00:00+24:00",
            "2026-01-01T00:00:00+00:60",
            "2026-01-01T00:00:00Zextra",
        ] {
            assert_eq!(Timestamp::parse(value), None, "{value}");
        }
    }

    #[test]
    fn canonical_turns_own_one_footer_even_with_steered_users_and_multiple_assistant_segments() {
        // turn-membership.test.ts:138 and layout.ts footer host selection.
        let rows = vec![
            user(1, 1, Some("a")),
            assistant(2, 2, Some("a")),
            tool(3, 3, Some("a"), "Read"),
            user(4, 4, Some("a")),
            assistant(5, 8, Some("a")),
            tool(6, 9, Some("b"), "heartbeat"),
            assistant(7, 12, Some("b")),
        ];
        let result = project(&rows, None, false, millis(20));
        assert_eq!(result.completed.len(), 2);
        assert_eq!(result.completed[0].host.id, 5);
        assert_eq!(result.completed[0].after.id, 5);
        assert_eq!(result.completed[0].response_range, 0..5);
        assert_eq!(
            result.completed[0].timing.as_ref().unwrap().duration_ms,
            Some(7000)
        );
        assert!(!result.completed[0].is_latest);
        assert_eq!(
            result.completed[1].timing.as_ref().unwrap().duration_ms,
            Some(3000)
        );
        assert!(result.completed[1].is_latest);
    }

    #[test]
    fn missing_ids_use_triggers_and_tool_only_turns_keep_raw_running_status() {
        let rows = vec![
            user(1, 1, None),
            assistant(2, 2, None),
            user(3, 3, None),
            tool(4, 5, None, "Read"),
            tool(5, 6, None, "heartbeat"),
            tool(6, 9, None, "Read"),
        ];
        let result = project(&rows, None, false, millis(10));
        assert_eq!(
            result
                .completed
                .iter()
                .map(|footer| footer.host.id)
                .collect::<Vec<_>>(),
            vec![2, 4, 6]
        );
        assert_eq!(
            result.completed[2].timing.as_ref().unwrap().duration_ms,
            Some(3000)
        );
        assert!(
            matches!(&*rows[5].item,TimelineItem::Tool(tool) if tool.status==crate::protocol::conversation::ToolStatus::Running)
        );
    }

    #[test]
    fn promptless_turns_show_completion_without_an_invented_duration() {
        let rows = vec![
            assistant(1, 2, Some("a")),
            tool(2, 5, Some("b"), "Read"),
            assistant(3, 9, Some("c")),
        ];
        let result = project(&rows, None, false, millis(10));
        assert_eq!(result.completed.len(), 3);
        assert!(
            result.completed.iter().all(|footer| footer
                .timing
                .as_ref()
                .unwrap()
                .duration_ms
                .is_none())
        );
        assert_eq!(
            result.completed[1]
                .timing
                .as_ref()
                .unwrap()
                .completed_at
                .unix_ms,
            millis(5)
        );
    }

    #[test]
    fn working_footer_has_one_owner_and_reconnect_start_time_without_requiring_output() {
        let current = Turn {
            turn_id: Some("b".into()),
            started_at: Some("2026-11-06T00:00:03Z".into()),
        };
        let rows = vec![
            user(1, 1, Some("a")),
            assistant(2, 2, Some("a")),
            user(3, 3, Some("b")),
            assistant(4, 4, Some("b")),
        ];
        let result = project(&rows, Some(&current), false, millis(8));
        assert_eq!(result.completed.len(), 1);
        assert_eq!(result.completed[0].host.id, 2);
        assert_eq!(result.running.unwrap().elapsed_ms, Some(5000));
        assert_eq!(
            project(&[], Some(&current), false, millis(8))
                .running
                .unwrap()
                .elapsed_ms,
            Some(5000)
        );
        assert_eq!(
            project(&[], None, true, millis(8))
                .running
                .unwrap()
                .elapsed_ms,
            None
        );
        assert!(project(&[], None, false, millis(8)).running.is_none());
        let completed = project(&rows, None, false, millis(8));
        assert_eq!(completed.completed.len(), 2);
        assert!(completed.running.is_none());
    }

    #[test]
    fn invalid_times_hide_only_timing_and_clock_skew_never_produces_negative_duration() {
        let mut rows = vec![user(1, 9, None), assistant(2, 2, None)];
        assert_eq!(
            project(&rows, None, false, millis(10)).completed[0]
                .timing
                .unwrap()
                .duration_ms,
            Some(0)
        );
        rows[0].timestamp = "invalid".into();
        assert_eq!(
            project(&rows, None, false, millis(10)).completed[0]
                .timing
                .unwrap()
                .duration_ms,
            None
        );
        rows[1].timestamp = "invalid".into();
        let result = project(&rows, None, false, millis(10));
        assert_eq!(result.completed.len(), 1);
        assert_eq!(result.completed[0].timing, None);
        let current = Turn {
            turn_id: None,
            started_at: Some("invalid".into()),
        };
        assert_eq!(
            project(&rows, Some(&current), false, millis(10))
                .running
                .unwrap()
                .started_at,
            None
        );
    }

    #[test]
    fn footer_follows_all_turn_content_without_crossing_the_next_turn_or_duplicating_latest() {
        let rows = vec![
            user(1, 1, None),
            assistant(2, 2, None),
            tool(3, 3, None, "Read"),
            row(4, 4, None, json!({"type":"notification","message":"done"})),
            user(5, 5, None),
            assistant(6, 6, None),
        ];
        let result = project(&rows, None, false, millis(10));
        assert_eq!(result.completed.len(), 2);
        assert_eq!(result.completed[0].host.id, 2);
        assert_eq!(result.completed[0].after.id, 4);
        assert_eq!(
            result.completed[0]
                .timing
                .as_ref()
                .unwrap()
                .completed_at
                .unix_ms,
            millis(4)
        );
        assert_eq!(
            result
                .completed
                .iter()
                .filter(|footer| footer.is_latest)
                .count(),
            1
        );
        assert!(
            project(&[user(1, 1, None)], None, false, millis(2))
                .completed
                .is_empty()
        );
        assert!(
            project(
                &[row(
                    1,
                    1,
                    None,
                    json!({"type":"reasoning","text":"thinking"})
                )],
                None,
                false,
                millis(2)
            )
            .completed
            .is_empty()
        );
    }

    #[test]
    fn prepending_history_and_replacing_projection_preserve_footer_row_keys() {
        let rows = vec![user(3, 3, Some("b")), assistant(4, 4, Some("b"))];
        let original = project(&rows, None, false, millis(10)).completed.remove(0);
        let mut expanded = vec![user(1, 1, Some("a")), assistant(2, 2, Some("a"))];
        expanded.extend(rows);
        let result = project(&expanded, None, false, millis(10));
        assert_eq!(result.completed[1].host, original.host);
        assert_eq!(result.completed[1].after, original.after);
        assert_eq!(result.completed[1].timing, original.timing);
        assert_eq!(result.completed[1].response_range, 2..4);
        expanded[3].item = Rc::new(
            serde_json::from_value(json!({"type":"assistant_message","text":"updated projection"}))
                .unwrap(),
        );
        assert_eq!(
            project(&expanded, None, false, millis(10)).completed[1],
            result.completed[1]
        );
    }

    #[test]
    fn agent_message_triggers_missing_id_turns_and_epoch_boundaries_never_share_a_footer() {
        let mut notification = tool(3, 3, None, "agent_message");
        let TimelineItem::Tool(call) = Rc::make_mut(&mut notification.item) else {
            panic!("not a tool")
        };
        call.extra
            .insert("agentMessage".into(), json!({"event":"message"}));
        let rows = vec![
            user(1, 1, None),
            assistant(2, 2, None),
            notification,
            assistant(4, 5, None),
        ];
        let result = project(&rows, None, false, millis(10));
        assert_eq!(result.completed.len(), 2);
        assert_eq!(result.completed[1].response_range, 2..4);
        assert_eq!(
            result.completed[1].timing.as_ref().unwrap().duration_ms,
            Some(2000)
        );

        let mut next = assistant(2, 8, Some("same-turn"));
        next.epoch = "new-epoch".into();
        let rows = [assistant(2, 2, Some("same-turn")), next];
        let result = project(&rows, None, false, millis(10));
        assert_eq!(result.completed.len(), 2);
        assert_ne!(result.completed[0].after, result.completed[1].after);
    }
}
