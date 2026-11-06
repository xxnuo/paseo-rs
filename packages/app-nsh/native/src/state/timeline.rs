//! Projected history and subscription/catch-up state, independent of transport and rendering.

use crate::protocol::conversation::{
    Collapse, Direction, History, Projection, Stream, TimelineEntry, TimelineItem,
    TimelineReplacement, ToolStatus,
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Unprepared,
    Bootstrapping,
    CatchingUp,
    Current,
    Detached,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) epoch: String,
    pub(crate) seq: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

impl SourceRange {
    fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) span: SourceRange,
    pub(crate) has_older: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub(crate) epoch: String,
    pub(crate) contiguous: Window,
    pub(crate) retained: Vec<Window>,
}

impl Coverage {
    fn end(&self) -> Position {
        Position {
            epoch: self.epoch.clone(),
            seq: self.contiguous.span.end,
        }
    }

    fn contains(&self, epoch: &str, seq: u64) -> bool {
        self.epoch == epoch
            && std::iter::once(&self.contiguous)
                .chain(&self.retained)
                .any(|window| window.span.start <= seq && seq <= window.span.end)
    }

    fn extend(&mut self, added: Window) {
        let anchor = self.contiguous.span.end;
        let mut windows = std::mem::take(&mut self.retained);
        windows.extend([self.contiguous.clone(), added]);
        windows.sort_by_key(|window| window.span.start);
        let mut merged: Vec<Window> = Vec::new();
        for window in windows {
            if let Some(previous) = merged.last_mut()
                && window.span.start <= previous.span.end.saturating_add(1)
            {
                if window.span.start == previous.span.start {
                    previous.has_older = window.has_older;
                }
                previous.span.end = previous.span.end.max(window.span.end);
            } else {
                merged.push(window);
            }
        }
        let index = merged
            .iter()
            .position(|window| window.span.start <= anchor && anchor <= window.span.end)
            .expect("the existing coverage remains loaded");
        self.contiguous = merged.remove(index);
        self.retained = merged;
    }
}

// No Debug: item contents, tool inputs and even unknown extensions can contain private data.
#[derive(Clone)]
pub(crate) struct Row {
    pub(crate) id: u64,
    pub(crate) epoch: String,
    pub(crate) provider: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) timestamp: String,
    pub(crate) span: SourceRange,
    pub(crate) sources: Vec<SourceRange>,
    pub(crate) collapsed: Rc<Vec<Collapse>>,
    pub(crate) item: Rc<TimelineItem>,
}

impl Row {
    fn intersects(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self
                .sources
                .iter()
                .any(|a| other.sources.iter().any(|b| a.overlaps(*b)))
    }

    fn same_identity(&self, other: &Self) -> bool {
        if self.epoch != other.epoch
            || self.provider != other.provider
            || self.turn_id != other.turn_id
        {
            return false;
        }
        match (&*self.item, &*other.item) {
            (TimelineItem::Tool(a), TimelineItem::Tool(b)) => a.call_id == b.call_id,
            (TimelineItem::User(a), TimelineItem::User(b)) => {
                a.client_message_id
                    .as_ref()
                    .zip(b.client_message_id.as_ref())
                    .is_some_and(|(a, b)| a == b)
                    || a.message_id
                        .as_ref()
                        .zip(b.message_id.as_ref())
                        .is_some_and(|(a, b)| a == b)
            }
            // Provider message IDs identify a message, not segments on either side of a tool.
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    Tail,
    After(Position),
    Before {
        cursor: Position,
        merge_window: bool,
    },
}

impl FetchPlan {
    fn matches(&self, direction: &Direction) -> bool {
        matches!(
            (self, direction),
            (Self::Tail, Direction::Tail)
                | (Self::After(_), Direction::After)
                | (Self::Before { .. }, Direction::Before)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Subscribe {
        generation: u64,
    },
    Fetch {
        generation: u64,
        request_id: String,
        plan: FetchPlan,
    },
}

struct Pending {
    request_id: String,
    plan: FetchPlan,
    buffer_watermark: u64,
    bootstrap: bool,
}

struct Buffered {
    arrival: u64,
    event: Stream,
}

pub(crate) struct Timeline {
    pub(crate) agent_id: String,
    pub(crate) status: Status,
    pub(crate) generation: u64,
    pub(crate) rows: Vec<Row>,
    pub(crate) coverage: Option<Coverage>,
    pub(crate) error: Option<String>,
    subscribed: bool,
    network_applied: bool,
    next_id: u64,
    next_request: u64,
    next_arrival: u64,
    pending: Option<Pending>,
    buffered: Vec<Buffered>,
}

impl Timeline {
    pub(crate) fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            status: Status::Unprepared,
            generation: 0,
            rows: Vec::new(),
            coverage: None,
            error: None,
            subscribed: false,
            network_applied: false,
            next_id: 0,
            next_request: 0,
            next_arrival: 0,
            pending: None,
            buffered: Vec::new(),
        }
    }

    /// Cache can paint history but cannot claim Current or overwrite a network response.
    pub(crate) fn paint_cache(
        &mut self,
        generation: u64,
        rows: Vec<Row>,
        coverage: Option<Coverage>,
    ) -> bool {
        if generation != self.generation || self.network_applied || !self.rows.is_empty() {
            return false;
        }
        self.next_id = rows.iter().map(|row| row.id).max().unwrap_or(0);
        self.rows = rows;
        self.coverage = coverage;
        true
    }

    pub(crate) fn attach(&mut self) -> Action {
        self.generation += 1;
        self.status = Status::Bootstrapping;
        self.subscribed = false;
        self.pending = None;
        self.error = None;
        Action::Subscribe {
            generation: self.generation,
        }
    }

    pub(crate) fn subscribed(&mut self, generation: u64) -> Vec<Action> {
        if generation != self.generation || self.status != Status::Bootstrapping || self.subscribed
        {
            return Vec::new();
        }
        self.subscribed = true;
        vec![self.fetch(self.resume_plan(), true)]
    }

    pub(crate) fn detach(&mut self) {
        self.generation += 1;
        self.status = Status::Detached;
        self.subscribed = false;
        self.pending = None;
    }

    fn resume_plan(&self) -> FetchPlan {
        self.coverage
            .as_ref()
            .map_or(FetchPlan::Tail, |range| FetchPlan::After(range.end()))
    }

    fn fetch(&mut self, plan: FetchPlan, bootstrap: bool) -> Action {
        self.next_request += 1;
        let request_id = format!(
            "timeline:{}:{}:{}",
            self.agent_id, self.generation, self.next_request
        );
        if !matches!(plan, FetchPlan::Before { .. }) {
            self.status = Status::CatchingUp;
        }
        self.pending = Some(Pending {
            request_id: request_id.clone(),
            plan: plan.clone(),
            buffer_watermark: self.next_arrival,
            bootstrap,
        });
        Action::Fetch {
            generation: self.generation,
            request_id,
            plan,
        }
    }

    pub(crate) fn before(&mut self, cursor: Position, merge_window: bool) -> Vec<Action> {
        if !self.subscribed
            || self.pending.is_some()
            || self.status != Status::Current
            || self
                .coverage
                .as_ref()
                .is_none_or(|range| range.epoch != cursor.epoch)
        {
            return Vec::new();
        }
        vec![self.fetch(
            FetchPlan::Before {
                cursor,
                merge_window,
            },
            false,
        )]
    }

    pub(crate) fn retry(&mut self) -> Vec<Action> {
        if self.status != Status::Failed {
            return Vec::new();
        }
        if !self.subscribed {
            return vec![self.attach()];
        }
        self.error = None;
        let plan = self
            .pending
            .as_ref()
            .map_or_else(|| self.resume_plan(), |pending| pending.plan.clone());
        let bootstrap = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.bootstrap);
        vec![self.fetch(plan, bootstrap)]
    }

    pub(crate) fn subscription_failed(&mut self, generation: u64, error: String) {
        if generation == self.generation && self.status == Status::Bootstrapping {
            self.status = Status::Failed;
            self.error = Some(error);
        }
    }

    /// shortcut: live payloads are buffered, add stream presentation when its reducer is ported.
    pub(crate) fn stream(&mut self, generation: u64, event: Stream) -> Vec<Action> {
        if generation != self.generation
            || event.agent_id != self.agent_id
            || matches!(self.status, Status::Unprepared | Status::Detached)
        {
            return Vec::new();
        }
        if event
            .epoch
            .as_deref()
            .zip(event.seq)
            .is_some_and(|(epoch, seq)| {
                self.coverage
                    .as_ref()
                    .is_some_and(|range| range.contains(epoch, seq))
            })
        {
            return Vec::new();
        }
        let epoch_changed = event
            .epoch
            .as_ref()
            .zip(self.coverage.as_ref())
            .is_some_and(|(epoch, range)| *epoch != range.epoch);
        self.next_arrival += 1;
        self.buffered.push(Buffered {
            arrival: self.next_arrival,
            event,
        });
        if self.subscribed && self.status != Status::Failed {
            self.status = Status::CatchingUp;
        }
        if !self.subscribed
            || self.status == Status::Failed
            || (self.pending.is_some() && !epoch_changed)
        {
            return Vec::new();
        }
        vec![self.fetch(
            if epoch_changed {
                FetchPlan::Tail
            } else {
                self.resume_plan()
            },
            false,
        )]
    }

    pub(crate) fn buffered_events(&self) -> impl Iterator<Item = &Stream> {
        self.buffered.iter().map(|buffered| &buffered.event)
    }

    pub(crate) fn replacement(
        &mut self,
        generation: u64,
        event: TimelineReplacement,
    ) -> Vec<Action> {
        if generation != self.generation || event.agent_id != self.agent_id || !self.subscribed {
            return Vec::new();
        }
        vec![self.fetch(FetchPlan::Tail, false)]
    }

    pub(crate) fn history(&mut self, generation: u64, history: History) -> Vec<Action> {
        if generation != self.generation || history.agent_id != self.agent_id || !self.subscribed {
            return Vec::new();
        }
        let Some(pending) = &self.pending else {
            return Vec::new();
        };
        if pending.request_id != history.request_id || !pending.plan.matches(&history.direction) {
            return Vec::new();
        }
        if let Some(error) = history.error.as_ref() {
            self.status = Status::Failed;
            self.error = Some(error.clone());
            return Vec::new();
        }
        if !valid_history(&history) {
            self.status = Status::Failed;
            self.error = Some("The daemon sent an invalid projected timeline".into());
            return Vec::new();
        }
        let pending = self.pending.take().expect("matched pending request");
        let before = matches!(pending.plan, FetchPlan::Before { .. });
        let epoch_changed = self
            .coverage
            .as_ref()
            .is_some_and(|range| range.epoch != history.epoch);
        if (history.reset || history.stale_cursor || history.gap || epoch_changed)
            && !matches!(pending.plan, FetchPlan::Tail)
        {
            // A before page or cursor repair is not a complete replacement snapshot.
            return vec![self.fetch(FetchPlan::Tail, false)];
        }
        let page = history
            .start_cursor
            .as_ref()
            .zip(history.end_cursor.as_ref())
            .map(|(start, end)| Window {
                span: SourceRange {
                    start: start.seq,
                    end: end.seq,
                },
                has_older: history.has_older,
            });
        if let (Some(range), Some(page)) = (&self.coverage, &page)
            && !matches!(pending.plan, FetchPlan::Tail)
        {
            let disconnected = if before {
                !matches!(
                    pending.plan,
                    FetchPlan::Before {
                        merge_window: true,
                        ..
                    }
                ) && page.span.end.saturating_add(1) != range.contiguous.span.start
            } else {
                page.span.start > range.contiguous.span.end.saturating_add(1)
            };
            if disconnected {
                return vec![self.fetch(self.resume_plan(), false)];
            }
        }
        if history.has_newer && (page.is_none() || page.as_ref().is_some_and(|page| {
            matches!(&pending.plan, FetchPlan::After(cursor) if page.span.end <= cursor.seq)
        })) {
            self.pending = Some(pending);
            self.status = Status::Failed;
            self.error = Some("Timeline catch-up did not advance its cursor".into());
            return Vec::new();
        }
        if matches!(pending.plan, FetchPlan::Tail) {
            let rewound = self
                .coverage
                .as_ref()
                .is_some_and(|range| history.window.max_seq < range.contiguous.span.end);
            if history.reset
                || epoch_changed
                || rewound
                || (history.entries.is_empty() && page.is_none())
            {
                self.rows.clear();
                self.coverage = None;
            }
            // A same-epoch tail keeps previously loaded pages, including disjoint history.
            self.buffered.retain(|buffered| {
                buffered.arrival > pending.buffer_watermark
                    || buffered
                        .event
                        .epoch
                        .as_deref()
                        .is_none_or(|epoch| epoch == history.epoch)
                        && (!rewound
                            || buffered
                                .event
                                .seq
                                .is_none_or(|seq| seq <= history.window.max_seq))
            });
        }
        for entry in history.entries {
            // Older projected daemons may return rows owned by the next before page.
            if before
                && page.as_ref().is_some_and(|page| {
                    entry.seq_start < page.span.start || entry.seq_start > page.span.end
                })
            {
                continue;
            }
            self.merge_entry(&history.epoch, entry);
        }
        self.rows.sort_by_key(|row| row.span.start);
        if let Some(page) = page {
            if let Some(range) = &mut self.coverage {
                range.extend(page);
            } else {
                self.coverage = Some(Coverage {
                    epoch: history.epoch.clone(),
                    contiguous: page,
                    retained: Vec::new(),
                });
            }
        }
        self.network_applied = true;
        self.error = None;
        if before {
            // Loading older rows cannot acknowledge a forward catch-up.
            if !self.buffered.is_empty() {
                return vec![self.fetch(self.resume_plan(), false)];
            }
            self.status = Status::Current;
            return Vec::new();
        }
        self.buffered.retain(|buffered| {
            match (buffered.event.epoch.as_deref(), buffered.event.seq) {
                (Some(epoch), Some(seq)) => !self
                    .coverage
                    .as_ref()
                    .is_some_and(|range| range.contains(epoch, seq)),
                _ => buffered.arrival > pending.buffer_watermark,
            }
        });
        if !history.has_newer
            && self.buffered.iter().any(|buffered| {
                buffered.arrival <= pending.buffer_watermark
                    && buffered.event.epoch.as_deref() == Some(history.epoch.as_str())
                    && buffered
                        .event
                        .seq
                        .is_some_and(|seq| seq > history.window.max_seq)
            })
        {
            self.pending = Some(pending);
            self.status = Status::Failed;
            self.error = Some("Timeline snapshot did not reach the buffered events".into());
            return Vec::new();
        }
        if history.has_newer
            || pending.bootstrap
                && matches!(pending.plan, FetchPlan::Tail)
                && self.coverage.is_some()
            || !self.buffered.is_empty()
            || self.coverage.as_ref().is_some_and(|range| {
                range
                    .retained
                    .iter()
                    .any(|window| window.span.start > range.contiguous.span.end)
            })
        {
            let epoch_pending = self.coverage.as_ref().is_some_and(|range| {
                self.buffered.iter().any(|buffered| {
                    buffered
                        .event
                        .epoch
                        .as_ref()
                        .is_some_and(|epoch| *epoch != range.epoch)
                })
            });
            return vec![self.fetch(
                if epoch_pending {
                    FetchPlan::Tail
                } else {
                    self.resume_plan()
                },
                false,
            )];
        }
        self.status = Status::Current;
        Vec::new()
    }

    fn merge_entry(&mut self, epoch: &str, entry: TimelineEntry) {
        let sources = if entry.source_seq_ranges.is_empty() {
            vec![SourceRange {
                start: entry.seq_start,
                end: entry.seq_end,
            }]
        } else {
            entry
                .source_seq_ranges
                .into_iter()
                .map(|range| SourceRange {
                    start: range.start_seq,
                    end: range.end_seq,
                })
                .collect()
        };
        self.next_id += 1;
        let mut incoming = Row {
            id: self.next_id,
            epoch: epoch.to_owned(),
            provider: entry.provider,
            turn_id: entry.turn_id,
            timestamp: entry.timestamp,
            span: SourceRange {
                start: entry.seq_start,
                end: entry.seq_end,
            },
            sources,
            collapsed: Rc::new(entry.collapsed),
            item: Rc::new(entry.item),
        };
        let mut retained_newer = None;
        self.rows.retain(|row| {
            if !row.intersects(&incoming) && !row.same_identity(&incoming) {
                return true;
            }
            incoming.id = incoming.id.min(row.id);
            let terminal_tool = matches!((&*row.item, &*incoming.item),
                (TimelineItem::Tool(old), TimelineItem::Tool(new))
                    if old.status != ToolStatus::Running && new.status == ToolStatus::Running);
            if row.span.end > incoming.span.end || terminal_tool {
                retained_newer = Some(row.clone());
            }
            false
        });
        self.rows.push(retained_newer.unwrap_or(incoming));
    }
}

fn valid_history(history: &History) -> bool {
    if history.projection != Projection::Projected
        || history.window.min_seq > history.window.max_seq.saturating_add(1)
        || history.window.next_seq <= history.window.max_seq
    {
        return false;
    }
    match (&history.start_cursor, &history.end_cursor) {
        (Some(start), Some(end))
            if start.epoch == history.epoch
                && end.epoch == history.epoch
                && start.seq <= end.seq => {}
        (None, None) if history.entries.is_empty() => {}
        _ => return false,
    }
    history.entries.iter().all(|entry| {
        entry.seq_start <= entry.seq_end
            && entry.source_seq_ranges.iter().all(|range| {
                entry.seq_start <= range.start_seq
                    && range.start_seq <= range.end_seq
                    && range.end_seq <= entry.seq_end
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn entry(start: u64, end: u64, item: Value, sources: &[(u64, u64)]) -> Value {
        json!({"provider":"mock","timestamp":"2026-11-06T00:00:00.000Z",
            "seqStart":start,"seqEnd":end,"item":item,"collapsed":[],
            "sourceSeqRanges":sources.iter().map(|(start,end)| json!({"startSeq":start,"endSeq":end})).collect::<Vec<_>>()})
    }

    fn assistant(start: u64, end: u64, text: &str) -> Value {
        entry(
            start,
            end,
            json!({"type":"assistant_message","text":text,"messageId":"answer"}),
            &[(start, end)],
        )
    }

    fn tool(start: u64, end: u64, status: &str, sources: &[(u64, u64)]) -> Value {
        entry(
            start,
            end,
            json!({"type":"tool_call","callId":"call","name":"Read",
            "status":status,"detail":{"type":"read","filePath":"/fixture/file"},"error":null}),
            sources,
        )
    }

    fn page(
        action: &Action,
        epoch: &str,
        start: Option<u64>,
        end: Option<u64>,
        entries: Vec<Value>,
    ) -> History {
        let Action::Fetch {
            request_id, plan, ..
        } = action
        else {
            panic!("not a fetch")
        };
        let (direction, merge) = match plan {
            FetchPlan::Tail => ("tail", false),
            FetchPlan::After(_) => ("after", false),
            FetchPlan::Before { merge_window, .. } => ("before", *merge_window),
        };
        serde_json::from_value(
            json!({"requestId":request_id,"agentId":"agent","agent":null,
            "direction":direction,"mergeWindow":merge,"projection":"projected","epoch":epoch,
            "reset":false,"staleCursor":false,"gap":false,
            "window":{"minSeq":1,"maxSeq":end.unwrap_or(0),"nextSeq":end.unwrap_or(0)+1},
            "startCursor":start.map(|seq| json!({"epoch":epoch,"seq":seq})),
            "endCursor":end.map(|seq| json!({"epoch":epoch,"seq":seq})),
            "hasOlder":false,"hasNewer":false,"entries":entries,"error":null}),
        )
        .unwrap()
    }

    fn start(timeline: &mut Timeline) -> Action {
        let Action::Subscribe { generation } = timeline.attach() else {
            panic!("not a subscription")
        };
        timeline.subscribed(generation).remove(0)
    }

    fn apply(timeline: &mut Timeline, action: &Action, history: History) -> Vec<Action> {
        let Action::Fetch { generation, .. } = action else {
            panic!("not a fetch")
        };
        timeline.history(*generation, history)
    }

    fn current(entries: Vec<Value>, end: u64) -> Timeline {
        let mut timeline = Timeline::new("agent".into());
        let tail = start(&mut timeline);
        let start = entries
            .first()
            .and_then(|entry| entry["seqStart"].as_u64())
            .unwrap_or(1);
        let after = apply(
            &mut timeline,
            &tail,
            page(&tail, "epoch", Some(start), Some(end), entries),
        )
        .remove(0);
        let mut empty = page(&after, "epoch", None, None, vec![]);
        empty.window.max_seq = end;
        empty.window.next_seq = end + 1;
        assert!(apply(&mut timeline, &after, empty).is_empty());
        assert_eq!(timeline.status, Status::Current);
        timeline
    }

    fn stream(epoch: Option<&str>, seq: Option<u64>) -> Stream {
        let mut value = json!({"agentId":"agent","timestamp":"2026-11-06T00:00:00.000Z",
            "event":{"type":"future_event","private":"never render as assistant"}});
        if let Some(epoch) = epoch {
            value["epoch"] = json!(epoch);
        }
        if let Some(seq) = seq {
            value["seq"] = json!(seq);
        }
        serde_json::from_value(value).unwrap()
    }

    fn text(row: &Row) -> &str {
        match &*row.item {
            TimelineItem::Assistant(text)
            | TimelineItem::Reasoning(text)
            | TimelineItem::User(text) => &text.text,
            _ => panic!("not text"),
        }
    }

    #[test]
    fn bootstrap_subscribes_before_fetch_and_rejects_late_generations_and_requests() {
        // viewed-timeline-sync owns generation; replica.test.ts:319 rejects late cache reads.
        let mut timeline = Timeline::new("agent".into());
        let old = start(&mut timeline);
        let next = start(&mut timeline);
        let old_page = page(
            &old,
            "epoch",
            Some(1),
            Some(1),
            vec![assistant(1, 1, "old")],
        );
        assert!(apply(&mut timeline, &old, old_page).is_empty());
        assert!(timeline.rows.is_empty());
        let response = page(
            &next,
            "epoch",
            Some(1),
            Some(1),
            vec![assistant(1, 1, "new")],
        );
        let after = apply(&mut timeline, &next, response).remove(0);
        assert!(!timeline.paint_cache(timeline.generation, vec![], None));
        assert!(matches!(
            after,
            Action::Fetch {
                plan: FetchPlan::After(_),
                ..
            }
        ));
        assert_eq!(timeline.status, Status::CatchingUp);
        let stale = page(
            &next,
            "epoch",
            Some(1),
            Some(1),
            vec![assistant(1, 1, "late")],
        );
        assert!(apply(&mut timeline, &next, stale).is_empty());
        assert_eq!(text(&timeline.rows[0]), "new");
        timeline.detach();
        assert!(timeline.subscribed(timeline.generation - 1).is_empty());
    }

    #[test]
    fn projected_overlap_replaces_text_and_preserves_disjoint_tool_sources() {
        // session-stream-reducers.test.ts:1856,1933,1964; replica.test.ts:266.
        let mut timeline = current(
            vec![tool(1, 1, "running", &[(1, 1)]), assistant(2, 2, "Hel")],
            2,
        );
        let assistant_id = timeline.rows[1].id;
        let action = timeline
            .stream(timeline.generation, stream(Some("epoch"), Some(4)))
            .remove(0);
        let response = page(
            &action,
            "epoch",
            Some(3),
            Some(4),
            vec![
                tool(1, 4, "completed", &[(1, 1), (4, 4)]),
                assistant(2, 3, "Hello"),
            ],
        );
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(text(&timeline.rows[1]), "Hello");
        assert_eq!(timeline.rows[1].id, assistant_id);
        assert_eq!(
            timeline.rows[0].sources,
            vec![
                SourceRange { start: 1, end: 1 },
                SourceRange { start: 4, end: 4 }
            ]
        );
        assert!(
            matches!(&*timeline.rows[0].item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Completed)
        );
    }

    #[test]
    fn shared_message_ids_do_not_merge_segments_around_tools_or_regress_completed_tools() {
        // session-stream-reducers.test.ts:732,950.
        let mut timeline = current(
            vec![
                assistant(1, 1, "before"),
                tool(2, 2, "completed", &[(2, 2)]),
                assistant(3, 3, "after"),
            ],
            3,
        );
        let action = timeline
            .before(
                Position {
                    epoch: "epoch".into(),
                    seq: 4,
                },
                true,
            )
            .remove(0);
        let response = page(
            &action,
            "epoch",
            Some(1),
            Some(3),
            vec![
                assistant(1, 1, "before"),
                tool(2, 2, "running", &[(2, 2)]),
                assistant(3, 3, "after"),
            ],
        );
        apply(&mut timeline, &action, response);
        assert_eq!(timeline.rows.len(), 3);
        assert_eq!(text(&timeline.rows[0]), "before");
        assert_eq!(text(&timeline.rows[2]), "after");
        assert!(
            matches!(&*timeline.rows[1].item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Completed)
        );
    }

    #[test]
    fn cache_and_seq_less_streams_never_claim_authoritative_coverage() {
        // replica.test.ts:191,266; session-stream-reducers.test.ts:1128.
        let cached = current(vec![assistant(1, 4, "cached prefix")], 4);
        let mut timeline = Timeline::new("agent".into());
        assert!(timeline.paint_cache(0, cached.rows, cached.coverage));
        assert_eq!(timeline.status, Status::Unprepared);
        let after = start(&mut timeline);
        assert!(
            matches!(&after, Action::Fetch { plan: FetchPlan::After(cursor), .. } if cursor.seq == 4)
        );
        assert!(
            timeline
                .stream(timeline.generation, stream(None, None))
                .is_empty()
        );
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 4);
        let response = page(
            &after,
            "epoch",
            Some(5),
            Some(5),
            vec![assistant(1, 5, "cached prefix completed")],
        );
        let repair = apply(&mut timeline, &after, response).remove(0);
        assert_eq!(timeline.status, Status::CatchingUp);
        let mut response = page(&repair, "epoch", None, None, vec![]);
        response.window.max_seq = 5;
        response.window.next_seq = 6;
        apply(&mut timeline, &repair, response);
        assert_eq!(timeline.status, Status::Current);
        assert_eq!(timeline.rows.len(), 1);
        assert_eq!(text(&timeline.rows[0]), "cached prefix completed");
        assert_eq!(timeline.buffered_events().count(), 0);
    }

    #[test]
    fn disjoint_before_windows_preserve_loaded_history_without_advancing_forward_cursor() {
        // session-stream-reducers.test.ts:2800 and projectedBeforePageOwnership.
        let mut timeline = current(vec![assistant(100, 110, "latest")], 110);
        let action = timeline
            .before(
                Position {
                    epoch: "epoch".into(),
                    seq: 41,
                },
                true,
            )
            .remove(0);
        let response = page(
            &action,
            "epoch",
            Some(1),
            Some(40),
            vec![assistant(1, 40, "older"), assistant(41, 50, "next-page")],
        );
        apply(&mut timeline, &action, response);
        let coverage = timeline.coverage.as_ref().unwrap();
        assert_eq!(
            coverage.contiguous.span,
            SourceRange {
                start: 100,
                end: 110
            }
        );
        assert_eq!(coverage.retained[0].span, SourceRange { start: 1, end: 40 });
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(timeline.status, Status::Current);
    }

    #[test]
    fn gaps_epochs_resets_replacements_and_errors_have_explicit_recovery() {
        // session-stream-reducers.test.ts:1507,1568.
        let mut timeline = current(vec![assistant(1, 2, "keep on error")], 2);
        let action = timeline
            .stream(timeline.generation, stream(Some("epoch"), Some(9)))
            .remove(0);
        let gap = page(
            &action,
            "epoch",
            Some(9),
            Some(9),
            vec![assistant(9, 9, "gap")],
        );
        let catch_up = apply(&mut timeline, &action, gap).remove(0);
        assert!(
            matches!(&catch_up, Action::Fetch { plan: FetchPlan::After(cursor), .. } if cursor.seq == 2)
        );
        assert_eq!(timeline.rows.len(), 1);
        let mut error = page(&catch_up, "epoch", None, None, vec![]);
        error.error = Some("history unavailable".into());
        apply(&mut timeline, &catch_up, error);
        assert_eq!(timeline.status, Status::Failed);
        assert_eq!(text(&timeline.rows[0]), "keep on error");
        let retry = timeline.retry().remove(0);
        let mut reset = page(&retry, "new-epoch", None, None, vec![]);
        reset.reset = true;
        let tail = apply(&mut timeline, &retry, reset).remove(0);
        assert!(matches!(
            tail,
            Action::Fetch {
                plan: FetchPlan::Tail,
                ..
            }
        ));
        let mut empty = page(&tail, "new-epoch", None, None, vec![]);
        empty.reset = true;
        apply(&mut timeline, &tail, empty);
        assert!(timeline.rows.is_empty());
        assert!(timeline.coverage.is_none());
        assert_eq!(timeline.status, Status::Current);
        let event = serde_json::from_value(json!({"agentId":"agent","epoch":"new-epoch"})).unwrap();
        assert!(matches!(
            timeline.replacement(timeline.generation, event)[0],
            Action::Fetch {
                plan: FetchPlan::Tail,
                ..
            }
        ));
    }

    #[test]
    fn a_before_page_does_not_clear_a_gap_that_arrives_while_it_is_loading() {
        let mut timeline = current(vec![assistant(3, 4, "latest")], 4);
        let action = timeline
            .before(
                Position {
                    epoch: "epoch".into(),
                    seq: 3,
                },
                false,
            )
            .remove(0);
        assert!(
            timeline
                .stream(timeline.generation, stream(Some("epoch"), Some(7)))
                .is_empty()
        );
        assert_eq!(timeline.status, Status::CatchingUp);
        let response = page(
            &action,
            "epoch",
            Some(1),
            Some(2),
            vec![assistant(1, 2, "older")],
        );
        let repair = apply(&mut timeline, &action, response).remove(0);
        assert_eq!(timeline.status, Status::CatchingUp);
        assert!(
            matches!(&repair, Action::Fetch { plan: FetchPlan::After(cursor), .. } if cursor.seq == 4)
        );
        let mut stalled = page(&repair, "epoch", None, None, vec![]);
        stalled.window.max_seq = 4;
        stalled.window.next_seq = 5;
        assert!(apply(&mut timeline, &repair, stalled).is_empty());
        assert_eq!(timeline.status, Status::Failed);
        assert_eq!(timeline.buffered_events().count(), 1);
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 4);
    }

    #[test]
    fn real_daemon_fixture_replays_without_inventing_live_text_or_duplicate_rows() {
        let capture: Vec<Value> =
            serde_json::from_str(include_str!("../../../protocol-fixtures/timeline.json")).unwrap();
        let mut timeline = Timeline::new("fixture-agent".into());
        let Action::Subscribe { generation } = timeline.attach() else {
            panic!("not a subscription")
        };
        for frame in &capture {
            let message = &frame["frame"]["message"];
            if message["type"] == "agent_stream" {
                assert!(
                    timeline
                        .stream(
                            generation,
                            serde_json::from_value(message["payload"].clone()).unwrap()
                        )
                        .is_empty()
                );
            }
        }
        assert!(timeline.rows.is_empty());
        let action = timeline.subscribed(generation).remove(0);
        let Action::Fetch { request_id, .. } = &action else {
            panic!("not a fetch")
        };
        let mut value = capture
            .iter()
            .find(|frame| frame["frame"]["message"]["type"] == "fetch_agent_timeline_response")
            .unwrap()["frame"]["message"]["payload"]
            .clone();
        value["requestId"] = json!(request_id);
        let after = apply(
            &mut timeline,
            &action,
            serde_json::from_value(value).unwrap(),
        )
        .remove(0);
        assert_eq!(timeline.rows.len(), 3);
        assert_eq!(text(&timeline.rows[2]), "FIXTURE_REPLY_中文");
        assert!(
            matches!(&*timeline.rows[1].item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Completed)
        );
        assert_eq!(timeline.buffered_events().count(), 0);
        let mut response = page(&after, "fixture-epoch", None, None, vec![]);
        response.agent_id = "fixture-agent".into();
        response.window.max_seq = 4;
        response.window.next_seq = 5;
        assert!(apply(&mut timeline, &after, response).is_empty());
        assert_eq!(timeline.status, Status::Current);
    }

    #[test]
    fn unknown_items_remain_typed_and_invalid_coverage_cannot_mutate_existing_history() {
        let mut timeline = current(
            vec![entry(
                1,
                1,
                json!({"type":"future_item","secret":"private"}),
                &[(1, 1)],
            )],
            1,
        );
        assert!(
            matches!(&*timeline.rows[0].item, TimelineItem::Unknown(value) if value["secret"] == "private")
        );
        let action = timeline
            .stream(timeline.generation, stream(Some("epoch"), Some(2)))
            .remove(0);
        let invalid = entry(
            2,
            2,
            json!({"type":"assistant_message","text":"private"}),
            &[(2, 3)],
        );
        apply(
            &mut timeline,
            &action,
            page(&action, "epoch", Some(2), Some(2), vec![invalid]),
        );
        assert_eq!(timeline.status, Status::Failed);
        assert_eq!(timeline.rows.len(), 1);
        assert!(!timeline.error.as_ref().unwrap().contains("private"));
    }
}
