//! Projected history and subscription/catch-up state, independent of transport and rendering.

use crate::protocol::conversation::{
    Collapse, Direction, History, KnownStreamEvent, Projection, Stream, StreamEvent, TimelineEntry,
    TimelineItem, TimelineReplacement, ToolCall, ToolDetail, ToolStatus,
};
use serde_json::Value;
use std::rc::Rc;

const MAX_BUFFERED_EVENTS: usize = 256;
const MAX_BUFFERED_BYTES: usize = 2 * 1024 * 1024;

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
    pub(crate) provisional: bool,
    pub(crate) active: bool,
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
        if !self.epoch.is_empty() && !other.epoch.is_empty() && self.epoch != other.epoch
            || self.provider != other.provider
        {
            return false;
        }
        match (&*self.item, &*other.item) {
            (TimelineItem::Tool(a), TimelineItem::Tool(b)) => {
                self.turn_id == other.turn_id && a.call_id == b.call_id
            }
            (TimelineItem::User(a), TimelineItem::User(b)) => {
                if let (Some(a), Some(b)) = (&a.client_message_id, &b.client_message_id) {
                    a == b
                } else {
                    a.message_id
                        .as_ref()
                        .zip(b.message_id.as_ref())
                        .is_some_and(|(a, b)| a == b)
                }
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
        operation_id: String,
        plan: FetchPlan,
    },
}

struct Pending {
    operation_id: String,
    plan: FetchPlan,
    buffer_watermark: u64,
    bootstrap: bool,
    provisional_rows: Vec<Row>,
}

struct Buffered {
    arrival: u64,
    event: Stream,
    row_id: Option<u64>,
    keep_provisional: bool,
}

struct OverlayBase {
    rows: Vec<Row>,
    coverage: Option<Coverage>,
    live_head: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    None,
    After,
    Tail,
}

pub(crate) struct Timeline {
    pub(crate) agent_id: String,
    pub(crate) status: Status,
    pub(crate) generation: u64,
    pub(crate) rows: Vec<Row>,
    pub(crate) coverage: Option<Coverage>,
    pub(crate) error: Option<String>,
    pub(crate) resync_required: bool,
    subscribed: bool,
    network_applied: bool,
    next_id: u64,
    next_request: u64,
    next_arrival: u64,
    pending: Option<Pending>,
    buffered: Vec<Buffered>,
    buffered_bytes: usize,
    overlay_base: Option<OverlayBase>,
    overflow_arrival: u64,
    completion_arrival: u64,
    live_head: Option<u64>,
    attachment_arrival: u64,
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
            resync_required: false,
            subscribed: false,
            network_applied: false,
            next_id: 0,
            next_request: 0,
            next_arrival: 0,
            pending: None,
            buffered: Vec::new(),
            buffered_bytes: 0,
            overlay_base: None,
            overflow_arrival: 0,
            completion_arrival: 0,
            live_head: None,
            attachment_arrival: 0,
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
        self.freeze_live();
        self.attachment_arrival = self.next_arrival;
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
        vec![self.fetch(self.recovery_plan(), true)]
    }

    pub(crate) fn detach(&mut self) {
        self.freeze_live();
        self.generation += 1;
        self.status = Status::Detached;
        self.subscribed = false;
        self.pending = None;
    }

    pub(crate) fn detach_if_generation(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.detach();
        true
    }

    fn freeze_live(&mut self) {
        self.live_head = None;
        for row in &mut self.rows {
            row.active = false;
        }
        if let Some(base) = &mut self.overlay_base {
            base.live_head = None;
            for row in &mut base.rows {
                row.active = false;
            }
        }
    }

    fn resume_plan(&self) -> FetchPlan {
        self.overlay_base
            .as_ref()
            .map_or(&self.coverage, |base| &base.coverage)
            .as_ref()
            .map_or(FetchPlan::Tail, |range| FetchPlan::After(range.end()))
    }

    fn fetch(&mut self, plan: FetchPlan, bootstrap: bool) -> Action {
        self.next_request += 1;
        let operation_id = format!(
            "timeline:{}:{}:{}",
            self.agent_id, self.generation, self.next_request
        );
        if !matches!(plan, FetchPlan::Before { .. }) {
            self.status = Status::CatchingUp;
        }
        self.pending = Some(Pending {
            operation_id: operation_id.clone(),
            plan: plan.clone(),
            buffer_watermark: self.next_arrival,
            bootstrap,
            provisional_rows: self
                .rows
                .iter()
                .filter(|row| row.provisional)
                .cloned()
                .collect(),
        });
        Action::Fetch {
            generation: self.generation,
            operation_id,
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
        let plan = if self.resync_required {
            FetchPlan::Tail
        } else {
            plan
        };
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

    pub(crate) fn history_failed(&mut self, generation: u64, operation_id: &str, error: String) {
        if generation == self.generation
            && self.subscribed
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.operation_id == operation_id)
        {
            self.status = Status::Failed;
            self.error = Some(error);
        }
    }

    pub(crate) fn refresh(&mut self, generation: u64) -> Vec<Action> {
        if generation != self.generation || !self.subscribed {
            return Vec::new();
        }
        self.error = None;
        vec![self.fetch(FetchPlan::Tail, false)]
    }

    pub(crate) fn catch_up(&mut self, generation: u64) -> Vec<Action> {
        if generation != self.generation || !self.subscribed {
            return Vec::new();
        }
        self.next_arrival += 1;
        self.completion_arrival = self.next_arrival;
        if self.pending.is_some() {
            return Vec::new();
        }
        self.error = None;
        vec![self.fetch(self.recovery_plan(), false)]
    }

    fn recovery_plan(&self) -> FetchPlan {
        let plan = self.resume_plan();
        let epoch_changed = match &plan {
            FetchPlan::After(cursor) => self.buffered.iter().any(|buffered| {
                buffered
                    .event
                    .epoch
                    .as_ref()
                    .is_some_and(|epoch| *epoch != cursor.epoch)
            }),
            _ => false,
        };
        if self.resync_required || epoch_changed {
            FetchPlan::Tail
        } else {
            plan
        }
    }

    pub(crate) fn stream(&mut self, generation: u64, event: Stream) -> Vec<Action> {
        if generation != self.generation
            || event.agent_id != self.agent_id
            || matches!(self.status, Status::Unprepared | Status::Detached)
        {
            return Vec::new();
        }
        self.next_arrival += 1;
        let recovery = self.observe(event, self.next_arrival);
        if !self.subscribed || self.status == Status::Failed || recovery == Recovery::None {
            return Vec::new();
        }
        if self.pending.is_some() {
            self.status = Status::CatchingUp;
            // An in-flight Tail already owns repair; do not starve it with every live frame.
            if recovery != Recovery::Tail
                || self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| matches!(pending.plan, FetchPlan::Tail))
            {
                return Vec::new();
            }
        }
        vec![self.fetch(
            if recovery == Recovery::Tail {
                FetchPlan::Tail
            } else {
                self.resume_plan()
            },
            false,
        )]
    }

    fn ensure_overlay(&mut self) {
        if self.overlay_base.is_none() {
            self.overlay_base = Some(OverlayBase {
                rows: self.rows.clone(),
                coverage: self.coverage.clone(),
                live_head: self.live_head,
            });
        }
    }

    fn buffer(&mut self, event: Stream, arrival: u64) -> bool {
        self.ensure_overlay();
        let size = buffered_size(&event);
        if self.buffered.len() >= MAX_BUFFERED_EVENTS
            || size > MAX_BUFFERED_BYTES.saturating_sub(self.buffered_bytes)
        {
            self.buffered.clear();
            self.buffered_bytes = 0;
            self.resync_required = true;
            self.overflow_arrival = self.overflow_arrival.max(arrival);
            return false;
        }
        self.buffered_bytes += size;
        self.buffered.push(Buffered {
            arrival,
            event,
            row_id: None,
            keep_provisional: false,
        });
        true
    }

    fn observe(&mut self, event: Stream, arrival: u64) -> Recovery {
        if matches!(
            &event.event,
            StreamEvent::Known(KnownStreamEvent::TurnStarted { .. })
        ) {
            self.live_head = None;
            if let Some(base) = &mut self.overlay_base {
                base.live_head = None;
            }
            for row in &mut self.rows {
                row.active = false;
            }
            if self.overlay_base.is_some() {
                self.buffer(event, arrival);
            }
            return Recovery::None;
        }
        let completion = matches!(
            &event.event,
            StreamEvent::Known(
                KnownStreamEvent::TurnCompleted { .. }
                    | KnownStreamEvent::TurnFailed { .. }
                    | KnownStreamEvent::TurnCanceled { .. }
            )
        );
        if completion {
            self.live_head = None;
            self.completion_arrival = arrival;
            for row in &mut self.rows {
                row.active = false;
            }
            return Recovery::After;
        }
        if self.resync_required {
            self.overflow_arrival = self.overflow_arrival.max(arrival);
            return Recovery::Tail;
        }
        let StreamEvent::Known(KnownStreamEvent::Timeline { .. }) = &event.event else {
            for row in &mut self.rows {
                if matches!(&*row.item, TimelineItem::Reasoning(_)) {
                    row.active = false;
                }
            }
            return if matches!(&event.event, StreamEvent::Unknown(_)) {
                if self.buffer(event, arrival) {
                    Recovery::After
                } else {
                    Recovery::Tail
                }
            } else {
                Recovery::None
            };
        };
        let position = event.epoch.as_deref().zip(event.seq);
        let mut provisional = !self.network_applied || position.is_none();
        if self.network_applied
            && let Some((epoch, seq)) = position
        {
            if let Some(coverage) = &self.coverage {
                if epoch != coverage.epoch {
                    self.buffer(event, arrival);
                    return Recovery::Tail;
                }
                if seq <= coverage.contiguous.span.end || coverage.contains(epoch, seq) {
                    return Recovery::None;
                }
                if seq != coverage.contiguous.span.end.saturating_add(1) {
                    return if self.buffer(event, arrival) {
                        Recovery::After
                    } else {
                        Recovery::Tail
                    };
                }
            } else if seq != 1 {
                self.buffer(event, arrival);
                return Recovery::Tail;
            }
        }
        if provisional || self.overlay_base.is_some() {
            if !self.buffer(event.clone(), arrival) {
                return Recovery::Tail;
            }
            provisional = true;
        }
        if self.network_applied
            && let Some((epoch, seq)) = position
        {
            if let Some(coverage) = &mut self.coverage {
                coverage.extend(Window {
                    span: SourceRange {
                        start: seq,
                        end: seq,
                    },
                    has_older: coverage.contiguous.has_older,
                });
            } else {
                self.coverage = Some(Coverage {
                    epoch: epoch.to_owned(),
                    contiguous: Window {
                        span: SourceRange {
                            start: seq,
                            end: seq,
                        },
                        has_older: false,
                    },
                    retained: Vec::new(),
                });
            }
        }
        let row_id = self.reduce_live(event, provisional);
        if let Some(buffered) = self
            .buffered
            .last_mut()
            .filter(|buffered| buffered.arrival == arrival)
        {
            buffered.row_id = row_id;
        }
        Recovery::None
    }

    fn reduce_live(&mut self, event: Stream, provisional: bool) -> Option<u64> {
        let StreamEvent::Known(KnownStreamEvent::Timeline {
            provider,
            item,
            turn_id,
        }) = event.event
        else {
            return None;
        };
        let reasoning = matches!(&item, TimelineItem::Reasoning(_));
        for row in &mut self.rows {
            row.active = false;
        }
        let position = event.epoch.as_ref().zip(event.seq);
        let sources = position.map_or_else(Vec::new, |(_, seq)| {
            vec![SourceRange {
                start: seq,
                end: seq,
            }]
        });
        self.next_id += 1;
        let mut incoming = Row {
            id: self.next_id,
            epoch: event.epoch.unwrap_or_default(),
            provider,
            turn_id,
            timestamp: event.timestamp,
            span: sources
                .first()
                .copied()
                .unwrap_or(SourceRange { start: 0, end: 0 }),
            sources,
            collapsed: Rc::new(Vec::new()),
            item: Rc::new(item),
            provisional,
            active: true,
        };
        let match_index = match &*incoming.item {
            TimelineItem::Assistant(text) | TimelineItem::Reasoning(text) => {
                let chunk = text.text.replace('\r', "");
                if chunk.is_empty() {
                    return None;
                }
                let matches_row = |index: usize| {
                    let row = &self.rows[index];
                    self.live_head == Some(row.id)
                        && row.provider == incoming.provider
                        && compatible_turn(row, &incoming)
                        && match (&*row.item, &*incoming.item) {
                            (TimelineItem::Assistant(old), TimelineItem::Assistant(new)) => {
                                new.message_id.is_none() || old.message_id == new.message_id
                            }
                            (TimelineItem::Reasoning(_), TimelineItem::Reasoning(_)) => {
                                row.turn_id == incoming.turn_id
                            }
                            _ => false,
                        }
                };
                let index = self
                    .rows
                    .len()
                    .checked_sub(1)
                    .filter(|&index| matches_row(index))
                    .or_else(|| {
                        (!reasoning
                            && self.rows.last().is_some_and(|row| {
                                row.provisional
                                    && row.sources.is_empty()
                                    && matches!(&*row.item, TimelineItem::User(_))
                            }))
                        .then(|| {
                            self.rows
                                .len()
                                .checked_sub(2)
                                .filter(|&index| matches_row(index))
                        })
                        .flatten()
                    });
                if index.is_none() && chunk.trim().is_empty() {
                    return None;
                }
                let mut text = text.clone();
                text.text = chunk;
                incoming.item = Rc::new(if reasoning {
                    TimelineItem::Reasoning(text)
                } else {
                    TimelineItem::Assistant(text)
                });
                index
            }
            TimelineItem::User(text) => {
                let normalized = text.text.replace('\r', "");
                if normalized.trim().is_empty() {
                    return None;
                }
                let index = self
                    .rows
                    .iter()
                    .position(|row| user_identity(row, &incoming));
                let mut text = text.clone();
                text.text = normalized;
                incoming.item = Rc::new(TimelineItem::User(text));
                index
            }
            TimelineItem::Tool(_) => self
                .rows
                .iter()
                .position(|row| row.same_identity(&incoming)),
            _ => None,
        };
        if let Some(index) = match_index {
            let previous = &self.rows[index];
            incoming.id = previous.id;
            if incoming.epoch.is_empty() {
                incoming.epoch = previous.epoch.clone();
            }
            incoming.provisional |= previous.provisional;
            incoming.sources = merge_sources(&previous.sources, &incoming.sources);
            incoming.span = span(&incoming.sources).unwrap_or(previous.span);
            match (&*previous.item, &*incoming.item) {
                (TimelineItem::Assistant(old), TimelineItem::Assistant(new))
                | (TimelineItem::Reasoning(old), TimelineItem::Reasoning(new)) => {
                    let mut text = new.clone();
                    text.text = format!("{}{}", old.text, new.text);
                    text.message_id = text.message_id.or_else(|| old.message_id.clone());
                    incoming.item = Rc::new(if reasoning {
                        TimelineItem::Reasoning(text)
                    } else {
                        TimelineItem::Assistant(text)
                    });
                }
                (TimelineItem::Tool(old), TimelineItem::Tool(new)) => {
                    incoming.item = Rc::new(TimelineItem::Tool(merge_tool(old, new)))
                }
                _ => {}
            }
            incoming.active = live_active(&incoming.item);
            let id = incoming.id;
            self.live_head = if matches!(
                &*incoming.item,
                TimelineItem::Assistant(_) | TimelineItem::Reasoning(_)
            ) {
                Some(id)
            } else if matches!(&*incoming.item, TimelineItem::Tool(_))
                && index + 1 < self.rows.len()
            {
                self.live_head
            } else if matches!(&*incoming.item, TimelineItem::User(_))
                && incoming.provisional
                && incoming.sources.is_empty()
            {
                self.live_head
            } else {
                None
            };
            self.rows[index] = incoming;
            Some(id)
        } else {
            incoming.active = live_active(&incoming.item);
            let id = incoming.id;
            self.live_head = if matches!(
                &*incoming.item,
                TimelineItem::Assistant(_) | TimelineItem::Reasoning(_)
            ) {
                Some(id)
            } else if matches!(&*incoming.item, TimelineItem::User(_))
                && incoming.provisional
                && incoming.sources.is_empty()
            {
                self.live_head
            } else {
                None
            };
            self.rows.push(incoming);
            Some(id)
        }
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

    // The client correlates its wire request ID; the owner returns our operation token separately.
    pub(crate) fn history(
        &mut self,
        generation: u64,
        operation_id: &str,
        history: History,
    ) -> Vec<Action> {
        if generation != self.generation || history.agent_id != self.agent_id || !self.subscribed {
            return Vec::new();
        }
        let Some(pending) = &self.pending else {
            return Vec::new();
        };
        if pending.operation_id != operation_id || !pending.plan.matches(&history.direction) {
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
        let current_coverage = self
            .overlay_base
            .as_ref()
            .map_or_else(|| self.coverage.clone(), |base| base.coverage.clone());
        let epoch_changed = current_coverage
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
        if let (Some(range), Some(page)) = (&current_coverage, &page)
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
        let previous_rows = self.overlay_base.as_ref().map(|_| self.rows.clone());
        if let Some(base) = self.overlay_base.take() {
            self.rows = base.rows;
            self.coverage = base.coverage;
            self.live_head = base.live_head;
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
                self.live_head = None;
            }
            // A same-epoch tail keeps previously loaded pages, including disjoint history.
            self.buffered.retain(|buffered| {
                if (history.reset || epoch_changed || rewound)
                    && buffered.arrival <= self.attachment_arrival
                    && (buffered.event.epoch.is_none() || buffered.event.seq.is_none())
                {
                    return false;
                }
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
        let base_end = current_coverage
            .as_ref()
            .map_or(0, |range| range.contiguous.span.end);
        let mut acknowledged_rows = std::collections::HashSet::new();
        let mut claimed_entries = std::collections::HashSet::new();
        for row in &pending.provisional_rows {
            if let Some((index, _)) = history.entries.iter().enumerate().find(|(index, entry)| {
                !claimed_entries.contains(index) && entry_covers_row(entry, row, base_end)
            }) {
                claimed_entries.insert(index);
                acknowledged_rows.insert(row.id);
            }
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
        if let Some(previous) = &previous_rows {
            self.reconcile_provisional_prefixes(previous, base_end);
        }
        if let Some(last) = self.rows.last().filter(|row| {
            row.span.end > base_end
                && matches!(
                    &*row.item,
                    TimelineItem::Assistant(_) | TimelineItem::Reasoning(_)
                )
        }) && self
            .buffered
            .iter()
            .any(|buffered| buffered.event.seq.is_some_and(|seq| seq > last.span.end))
        {
            self.live_head = Some(last.id);
        }
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
        if !before {
            self.buffered.retain(|buffered| {
                match (buffered.event.epoch.as_deref(), buffered.event.seq) {
                    (Some(epoch), Some(seq)) => !self
                        .coverage
                        .as_ref()
                        .is_some_and(|range| range.contains(epoch, seq)),
                    _ => {
                        matches!(
                            &buffered.event.event,
                            StreamEvent::Known(KnownStreamEvent::TurnStarted { .. })
                        ) || buffered.keep_provisional
                            || buffered.arrival > pending.buffer_watermark
                            || buffered
                                .row_id
                                .is_some_and(|id| !acknowledged_rows.contains(&id))
                    }
                }
            });
            if !history.has_newer && self.overflow_arrival <= pending.buffer_watermark {
                self.resync_required = false;
            }
        }
        let remaining = std::mem::take(&mut self.buffered);
        self.buffered_bytes = 0;
        let mut recovery = Recovery::None;
        let mut replaying_previous_attachment = false;
        for buffered in remaining {
            let old_attachment = buffered.arrival <= self.attachment_arrival;
            if replaying_previous_attachment && !old_attachment {
                self.freeze_live();
            }
            replaying_previous_attachment = old_attachment;
            match self.observe(buffered.event, buffered.arrival) {
                Recovery::Tail => recovery = Recovery::Tail,
                Recovery::After if recovery != Recovery::Tail => recovery = Recovery::After,
                _ => {}
            }
        }
        if replaying_previous_attachment {
            self.freeze_live();
        }
        if let Some(previous) = previous_rows {
            self.restore_row_ids(&previous);
        }
        if !before
            && !history.has_newer
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
        let owed_completion = self.completion_arrival > pending.buffer_watermark;
        if self.completion_arrival > 0
            && self
                .buffered
                .iter()
                .all(|buffered| buffered.arrival <= self.completion_arrival)
        {
            self.live_head = None;
            for row in &mut self.rows {
                row.active = false;
            }
        }
        if !owed_completion {
            self.completion_arrival = 0;
        }
        if !before && history.has_newer
            || pending.bootstrap
                && matches!(pending.plan, FetchPlan::Tail)
                && self.coverage.is_some()
            || pending.bootstrap && !self.buffered.is_empty()
            || recovery != Recovery::None
            || self.resync_required
            || owed_completion
            || !before
                && self.coverage.as_ref().is_some_and(|range| {
                    range
                        .retained
                        .iter()
                        .any(|window| window.span.start > range.contiguous.span.end)
                })
        {
            return vec![self.fetch(
                if recovery == Recovery::Tail {
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

    fn restore_row_ids(&mut self, previous: &[Row]) {
        let mut used = std::collections::HashSet::new();
        let mut reassigned = std::collections::HashMap::new();
        for row in &mut self.rows {
            let generated = row.id;
            if let Some(old) = previous
                .iter()
                .find(|old| old.id == generated && !used.contains(&old.id))
                .or_else(|| {
                    previous
                        .iter()
                        .find(|old| !used.contains(&old.id) && presentation_match(old, row))
                })
            {
                row.id = old.id;
            }
            if !used.insert(row.id) {
                self.next_id += 1;
                row.id = self.next_id;
                used.insert(row.id);
            }
            reassigned.insert(generated, row.id);
        }
        for buffered in &mut self.buffered {
            if let Some(id) = buffered.row_id.and_then(|id| reassigned.get(&id)) {
                buffered.row_id = Some(*id);
            }
        }
        if let Some(id) = self.live_head.and_then(|id| reassigned.get(&id)) {
            self.live_head = Some(*id);
        }
    }

    fn reconcile_provisional_prefixes(&mut self, previous: &[Row], base_end: u64) {
        let mut claimed = std::collections::HashSet::new();
        for live in previous.iter().filter(|row| row.provisional) {
            let (TimelineItem::Assistant(text) | TimelineItem::Reasoning(text)) = &*live.item
            else {
                continue;
            };
            let candidates: Vec<usize> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(index, canonical)| {
                    !claimed.contains(index)
                        && canonical.span.end > base_end
                        && canonical.provider == live.provider
                        && compatible_turn(canonical, live)
                        && (canonical.intersects(live)
                            || index + 1 == self.rows.len()
                            || match (&*canonical.item, &*live.item) {
                                (TimelineItem::Assistant(a), TimelineItem::Assistant(b)) => a
                                    .message_id
                                    .as_ref()
                                    .zip(b.message_id.as_ref())
                                    .is_some_and(|(a, b)| a == b),
                                _ => false,
                            })
                        && match (&*canonical.item, &*live.item) {
                            (TimelineItem::Assistant(a), TimelineItem::Assistant(b))
                            | (TimelineItem::Reasoning(a), TimelineItem::Reasoning(b)) => {
                                !a.message_id
                                    .as_ref()
                                    .zip(b.message_id.as_ref())
                                    .is_some_and(|(a, b)| a != b)
                                    && (a.text.starts_with(&b.text) || b.text.starts_with(&a.text))
                            }
                            _ => false,
                        }
                })
                .map(|(index, _)| index)
                .collect();
            if candidates.len() != 1 {
                continue;
            }
            let index = candidates[0];
            let (TimelineItem::Assistant(canonical) | TimelineItem::Reasoning(canonical)) =
                &*self.rows[index].item
            else {
                continue;
            };
            let suffix = text
                .text
                .strip_prefix(&canonical.text)
                .unwrap_or("")
                .to_owned();
            let mut matching: Vec<usize> = self
                .buffered
                .iter()
                .enumerate()
                .filter(|(_, buffered)| {
                    buffered.row_id == Some(live.id)
                        && (buffered.event.seq.is_none() || buffered.event.epoch.is_none())
                })
                .map(|(index, _)| index)
                .collect();
            if matching.is_empty() {
                continue;
            }
            claimed.insert(index);
            let canonical_id = self.rows[index].id;
            self.rows[index].id = live.id;
            if self.live_head == Some(canonical_id) {
                self.live_head = Some(live.id);
            }
            let retained = if suffix.is_empty() {
                None
            } else {
                matching.pop()
            };
            for index in matching {
                self.buffered[index].row_id = None;
                self.buffered[index].arrival = 0;
            }
            if let Some(index) = retained {
                if let StreamEvent::Known(KnownStreamEvent::Timeline {
                    item: TimelineItem::Assistant(text) | TimelineItem::Reasoning(text),
                    ..
                }) = &mut self.buffered[index].event.event
                {
                    text.text = suffix;
                    self.buffered[index].keep_provisional = true;
                    self.live_head = Some(self.rows[candidates[0]].id);
                }
            }
        }
        self.buffered.retain(|buffered| buffered.arrival != 0);
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
            provisional: false,
            active: false,
        };
        if matches!(&*incoming.item, TimelineItem::User(_))
            && self
                .rows
                .iter()
                .any(|row| Some(row.id) == self.live_head && row.span.end <= incoming.span.end)
        {
            self.live_head = None;
        }
        let mut retained_newer = None;
        self.rows.retain(|row| {
            if !row.intersects(&incoming) && !row.same_identity(&incoming) {
                return true;
            }
            incoming.id = incoming.id.min(row.id);
            if let (TimelineItem::Tool(old), TimelineItem::Tool(new)) =
                (&*row.item, &*incoming.item)
            {
                incoming.item = Rc::new(TimelineItem::Tool(merge_tool(old, new)));
                incoming.sources = merge_sources(&row.sources, &incoming.sources);
                incoming.span = span(&incoming.sources).unwrap_or(incoming.span);
                return false;
            }
            if row.span.end > incoming.span.end {
                retained_newer = Some(row.clone());
            }
            false
        });
        self.rows.push(retained_newer.unwrap_or(incoming));
    }
}

fn compatible_turn(left: &Row, right: &Row) -> bool {
    left.turn_id.is_none() || right.turn_id.is_none() || left.turn_id == right.turn_id
}

fn live_active(item: &TimelineItem) -> bool {
    matches!(
        item,
        TimelineItem::Assistant(_) | TimelineItem::Reasoning(_)
    ) || matches!(item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Running)
}

fn user_identity(left: &Row, right: &Row) -> bool {
    if left.provider != right.provider
        || !left.epoch.is_empty() && !right.epoch.is_empty() && left.epoch != right.epoch
    {
        return false;
    }
    match (&*left.item, &*right.item) {
        (TimelineItem::User(a), TimelineItem::User(b)) => {
            if let (Some(a), Some(b)) = (&a.client_message_id, &b.client_message_id) {
                a == b
            } else {
                a.message_id
                    .as_ref()
                    .zip(b.message_id.as_ref())
                    .is_some_and(|(a, b)| a == b)
            }
        }
        _ => false,
    }
}

fn presentation_match(left: &Row, right: &Row) -> bool {
    if left.intersects(right) || user_identity(left, right) || left.same_identity(right) {
        return true;
    }
    if !left.provisional
        || !left.sources.is_empty()
        || !right.sources.is_empty()
        || left.provider != right.provider
        || left.turn_id != right.turn_id
    {
        return false;
    }
    match (&*left.item, &*right.item) {
        (TimelineItem::Assistant(a), TimelineItem::Assistant(b))
        | (TimelineItem::Reasoning(a), TimelineItem::Reasoning(b)) => {
            (a.message_id.is_none() || b.message_id.is_none() || a.message_id == b.message_id)
                && (a.text.starts_with(&b.text) || b.text.starts_with(&a.text))
        }
        _ => false,
    }
}

fn entry_covers_row(entry: &TimelineEntry, row: &Row, base_end: u64) -> bool {
    if entry.provider != row.provider
        || entry
            .turn_id
            .as_ref()
            .zip(row.turn_id.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        return false;
    }
    match (&entry.item, &*row.item) {
        (TimelineItem::Assistant(canonical), TimelineItem::Assistant(live))
        | (TimelineItem::Reasoning(canonical), TimelineItem::Reasoning(live)) => {
            if canonical
                .message_id
                .as_ref()
                .zip(live.message_id.as_ref())
                .is_some_and(|(a, b)| a != b)
            {
                return false;
            }
            canonical.text.starts_with(&live.text)
                || entry.seq_end > base_end
                    && row.sources.iter().any(|source| {
                        entry.source_seq_ranges.iter().any(|range| {
                            source.overlaps(SourceRange {
                                start: range.start_seq,
                                end: range.end_seq,
                            })
                        })
                    })
        }
        (TimelineItem::User(canonical), TimelineItem::User(live)) => {
            if let (Some(a), Some(b)) = (&canonical.client_message_id, &live.client_message_id) {
                a == b
            } else {
                canonical
                    .message_id
                    .as_ref()
                    .zip(live.message_id.as_ref())
                    .is_some_and(|(a, b)| a == b)
            }
        }
        (TimelineItem::Tool(canonical), TimelineItem::Tool(live)) => {
            canonical.call_id == live.call_id
                && merge_tool(live, canonical).status == canonical.status
        }
        (TimelineItem::Unknown(canonical), TimelineItem::Unknown(live))
        | (TimelineItem::OtherKnown(canonical), TimelineItem::OtherKnown(live)) => {
            canonical == live
        }
        _ => false,
    }
}

fn merge_sources(left: &[SourceRange], right: &[SourceRange]) -> Vec<SourceRange> {
    let mut ranges = [left, right].concat();
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<SourceRange> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end.saturating_add(1)
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn span(ranges: &[SourceRange]) -> Option<SourceRange> {
    Some(SourceRange {
        start: ranges.iter().map(|range| range.start).min()?,
        end: ranges.iter().map(|range| range.end).max()?,
    })
}

fn merge_tool(existing: &ToolCall, incoming: &ToolCall) -> ToolCall {
    let mut merged = incoming.clone();
    merged.status = match (&existing.status, &incoming.status) {
        (ToolStatus::Failed, _) | (_, ToolStatus::Failed) => ToolStatus::Failed,
        (ToolStatus::Canceled, _) => ToolStatus::Canceled,
        (ToolStatus::Completed, _) => ToolStatus::Completed,
        (_, ToolStatus::Canceled) => ToolStatus::Canceled,
        (_, ToolStatus::Completed) => ToolStatus::Completed,
        _ => ToolStatus::Running,
    };
    merged.error = if merged.status == ToolStatus::Failed {
        if !incoming.error.is_null() {
            incoming.error.clone()
        } else {
            existing.error.clone()
        }
    } else {
        Value::Null
    };
    merged.detail = merge_detail(&existing.detail, &incoming.detail);
    let mut metadata = existing.metadata.clone().unwrap_or_default();
    metadata.extend(incoming.metadata.clone().unwrap_or_default());
    merged.metadata = if metadata.is_empty() {
        None
    } else {
        Some(metadata)
    };
    let mut extra = existing.extra.clone();
    extra.extend(incoming.extra.clone());
    merged.extra = extra;
    merged
}

fn merge_detail(existing: &ToolDetail, incoming: &ToolDetail) -> ToolDetail {
    if incoming.kind == "unknown" && existing.kind != "unknown" {
        return existing.clone();
    }
    if incoming.kind != existing.kind {
        return incoming.clone();
    }
    let mut merged = existing.clone();
    for (key, value) in &incoming.fields {
        if incoming.kind == "unknown" && matches!(key.as_str(), "input" | "output") {
            let previous = existing.fields.get(key);
            if value.is_null()
                || previous.is_some_and(|old| old.as_object().is_some_and(|map| !map.is_empty()))
                    && value.as_object().is_none_or(|map| map.is_empty())
            {
                continue;
            }
        }
        merged.fields.insert(key.clone(), value.clone());
    }
    merged
}

fn value_size(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items
            .iter()
            .fold(32usize, |size, item| size.saturating_add(value_size(item))),
        Value::Object(fields) => fields.iter().fold(32usize, |size, (key, value)| {
            size.saturating_add(key.len())
                .saturating_add(value_size(value))
        }),
        _ => 16,
    }
}

fn buffered_size(event: &Stream) -> usize {
    let payload = match &event.event {
        StreamEvent::Unknown(value) => value_size(value),
        StreamEvent::Known(KnownStreamEvent::Timeline {
            item,
            provider,
            turn_id,
        }) => {
            let size = match item {
                TimelineItem::User(text)
                | TimelineItem::Assistant(text)
                | TimelineItem::Reasoning(text) => text
                    .text
                    .len()
                    .saturating_add(text.message_id.as_ref().map_or(0, String::len))
                    .saturating_add(text.client_message_id.as_ref().map_or(0, String::len))
                    .saturating_add(text.extra.iter().fold(0usize, |size, (key, value)| {
                        size.saturating_add(key.len())
                            .saturating_add(value_size(value))
                    })),
                TimelineItem::Tool(tool) => tool
                    .call_id
                    .len()
                    .saturating_add(tool.name.len())
                    .saturating_add(
                        tool.detail
                            .fields
                            .iter()
                            .chain(tool.extra.iter())
                            .chain(tool.metadata.iter().flat_map(|metadata| metadata.iter()))
                            .fold(0usize, |size, (key, value)| {
                                size.saturating_add(key.len())
                                    .saturating_add(value_size(value))
                            }),
                    )
                    .saturating_add(value_size(&tool.error)),
                TimelineItem::Unknown(value) | TimelineItem::OtherKnown(value) => value_size(value),
            };
            size.saturating_add(provider.len())
                .saturating_add(turn_id.as_ref().map_or(0, String::len))
        }
        _ => 0,
    };
    payload
        .saturating_add(event.agent_id.len())
        .saturating_add(event.timestamp.len())
        .saturating_add(event.epoch.as_ref().map_or(0, String::len))
        .saturating_add(event.subscription_id.as_ref().map_or(0, String::len))
        .saturating_add(128)
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
        let Action::Fetch { plan, .. } = action else {
            panic!("not a fetch")
        };
        let (direction, merge) = match plan {
            FetchPlan::Tail => ("tail", false),
            FetchPlan::After(_) => ("after", false),
            FetchPlan::Before { merge_window, .. } => ("before", *merge_window),
        };
        serde_json::from_value(
            json!({"requestId":"7f56b035-8326-45b2-a45c-7680d10f2404","agentId":"agent","agent":null,
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
        let Action::Fetch {
            generation,
            operation_id,
            ..
        } = action
        else {
            panic!("not a fetch")
        };
        timeline.history(*generation, operation_id, history)
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

    fn live(item: Value, seq: Option<u64>) -> Stream {
        let mut value = json!({"agentId":"agent","epoch":"epoch","timestamp":"2026-11-06T00:00:00.000Z",
            "event":{"type":"timeline","provider":"mock","item":item}});
        if let Some(seq) = seq {
            value["seq"] = json!(seq);
        }
        serde_json::from_value(value).unwrap()
    }

    fn delta(text: &str, seq: Option<u64>) -> Stream {
        live(
            json!({"type":"assistant_message","text":text,"messageId":"answer"}),
            seq,
        )
    }

    fn finish() -> Stream {
        serde_json::from_value(
            json!({"agentId":"agent","timestamp":"2026-11-06T00:00:00.000Z",
            "event":{"type":"turn_completed","provider":"mock"}}),
        )
        .unwrap()
    }

    fn user(seq: u64) -> Value {
        entry(
            seq,
            seq,
            json!({"type":"user_message","text":"prompt","clientMessageId":"prompt"}),
            &[(seq, seq)],
        )
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
    fn reconnect_preserves_all_loaded_rows_and_rejects_the_old_page_cleanup() {
        let mut timeline = current(seeded_history(), 128);
        let ids: Vec<_> = timeline.rows.iter().map(|row| row.id).collect();
        let old_generation = timeline.generation;
        assert!(timeline.detach_if_generation(old_generation));
        let after = start(&mut timeline);
        assert!(matches!(
            &after,
            Action::Fetch {
                plan: FetchPlan::After(Position { seq: 128, .. }),
                ..
            }
        ));
        assert!(!timeline.detach_if_generation(old_generation));
        let mut response = page(&after, "epoch", None, None, vec![]);
        response.window.max_seq = 128;
        response.window.next_seq = 129;
        assert!(apply(&mut timeline, &after, response).is_empty());
        assert_eq!(timeline.status, Status::Current);
        assert_eq!(
            timeline.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(text(&timeline.rows[0]), "NSH_HISTORY_USER_000");
        assert_eq!(text(&timeline.rows[127]), "NSH_HISTORY_REPLY_063");
    }

    #[test]
    fn reconnect_freezes_the_previous_live_segment_and_new_epoch_drops_old_provisional() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, anonymous("old answer", Some(2)));
        timeline.detach();
        let _ = start(&mut timeline);
        assert!(timeline.rows.iter().all(|row| !row.active));
        timeline.stream(timeline.generation, anonymous("new answer", Some(3)));
        assert_eq!(timeline.rows.len(), 3);
        assert_eq!(text(&timeline.rows[1]), "old answer");
        assert_eq!(text(&timeline.rows[2]), "new answer");

        let mut timeline = current(vec![user(1)], 1);
        let mut provisional = anonymous("old uncommitted reply", None);
        provisional.epoch = None;
        timeline.stream(timeline.generation, provisional);
        timeline.detach();
        let after = start(&mut timeline);
        let mut response = page(&after, "new-epoch", Some(1), Some(1), vec![user(1)]);
        response.reset = true;
        let tail = apply(&mut timeline, &after, response).remove(0);
        let mut response = page(&tail, "new-epoch", Some(1), Some(1), vec![user(1)]);
        response.reset = true;
        let next = apply(&mut timeline, &tail, response);
        assert!(
            timeline
                .rows
                .iter()
                .all(|row| text(row) != "old uncommitted reply")
        );
        assert!(timeline.rows.iter().all(|row| row.epoch == "new-epoch"));
        assert!(
            next.iter()
                .all(|action| matches!(action, Action::Fetch { .. }))
        );
    }

    #[test]
    fn old_provisional_replay_stays_frozen_after_an_empty_same_epoch_catch_up() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, anonymous("old ", None));
        timeline.stream(timeline.generation, anonymous("answer", None));
        timeline.detach();
        let after = start(&mut timeline);
        let mut response = page(&after, "epoch", None, None, vec![]);
        response.window.max_seq = 1;
        response.window.next_seq = 2;
        apply(&mut timeline, &after, response);
        assert_eq!(text(&timeline.rows[1]), "old answer");
        assert!(timeline.rows.iter().all(|row| !row.active));
        timeline.stream(timeline.generation, anonymous("new answer", None));
        assert_eq!(timeline.rows.len(), 3);
        assert_eq!(text(&timeline.rows[1]), "old answer");
        assert_eq!(text(&timeline.rows[2]), "new answer");
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
        assert_eq!(timeline.rows.len(), 3);
        let action = timeline.subscribed(generation).remove(0);
        let value = capture
            .iter()
            .find(|frame| frame["frame"]["message"]["type"] == "fetch_agent_timeline_response")
            .unwrap()["frame"]["message"]["payload"]
            .clone();
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

    #[test]
    fn operation_tokens_are_independent_of_wire_request_ids_and_reject_stale_operations() {
        let mut timeline = Timeline::new("agent".into());
        let tail = start(&mut timeline);
        let Action::Fetch {
            operation_id: old_operation,
            ..
        } = &tail
        else {
            panic!("not a fetch")
        };
        let response = page(
            &tail,
            "epoch",
            Some(1),
            Some(1),
            vec![assistant(1, 1, "accepted")],
        );
        assert_ne!(response.request_id, *old_operation);
        let after = apply(&mut timeline, &tail, response).remove(0);
        let mut stale = page(&after, "epoch", None, None, vec![]);
        stale.window.max_seq = 1;
        stale.window.next_seq = 2;
        assert!(
            timeline
                .history(timeline.generation, old_operation, stale)
                .is_empty()
        );
        assert_eq!(timeline.status, Status::CatchingUp);
        let mut response = page(&after, "epoch", None, None, vec![]);
        response.window.max_seq = 1;
        response.window.next_seq = 2;
        assert!(apply(&mut timeline, &after, response).is_empty());
        assert_eq!(timeline.status, Status::Current);
        assert_eq!(text(&timeline.rows[0]), "accepted");
    }

    #[test]
    fn user_client_ids_distinguish_independent_sends_with_the_same_provider_message_id() {
        let user = |seq, client: Option<&str>| {
            let mut item =
                json!({"type":"user_message","messageId":"shared-provider-id","text":"same text"});
            if let Some(client) = client {
                item["clientMessageId"] = json!(client);
            }
            entry(seq, seq, item, &[(seq, seq)])
        };
        let mut timeline = current(
            vec![user(1, Some("first-send")), user(2, Some("second-send"))],
            2,
        );
        assert_eq!(timeline.rows.len(), 2);
        assert!(
            matches!(&*timeline.rows[0].item, TimelineItem::User(user) if user.client_message_id.as_deref() == Some("first-send"))
        );
        assert!(
            matches!(&*timeline.rows[1].item, TimelineItem::User(user) if user.client_message_id.as_deref() == Some("second-send"))
        );
        let first_id = timeline.rows[0].id;
        timeline.merge_entry(
            "epoch",
            serde_json::from_value(user(3, Some("first-send"))).unwrap(),
        );
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(timeline.rows.last().unwrap().id, first_id);

        let mut legacy = current(vec![user(1, None)], 1);
        let legacy_id = legacy.rows[0].id;
        legacy.merge_entry(
            "epoch",
            serde_json::from_value(user(2, Some("first-send"))).unwrap(),
        );
        assert_eq!(legacy.rows.len(), 1);
        assert_eq!(legacy.rows[0].id, legacy_id);
    }

    #[test]
    fn committed_chunks_render_immediately_and_only_contiguous_sequences_advance() {
        // session-stream-reducers.ts sequencing gate; agent-manager.test.ts committed coalescer.
        let mut timeline = current(vec![user(1)], 1);
        assert!(
            timeline
                .stream(timeline.generation, delta("Hel", Some(2)))
                .is_empty()
        );
        let id = timeline.rows[1].id;
        assert_eq!(text(&timeline.rows[1]), "Hel");
        assert!(!timeline.rows[1].provisional);
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 2);
        assert!(
            timeline
                .stream(timeline.generation, delta("Hel", Some(2)))
                .is_empty()
        );
        assert_eq!(text(&timeline.rows[1]), "Hel");
        assert!(
            timeline
                .stream(timeline.generation, delta("lo\r", Some(3)))
                .is_empty()
        );
        assert_eq!(text(&timeline.rows[1]), "Hello");
        assert_eq!(timeline.rows[1].id, id);
        for (seq, chunk) in [(4, "Thi"), (5, "nk")] {
            assert!(
                timeline
                    .stream(
                        timeline.generation,
                        live(json!({"type":"reasoning","text":chunk}), Some(seq))
                    )
                    .is_empty()
            );
        }
        assert_eq!(text(&timeline.rows[2]), "Think");
        assert!(timeline.rows[2].active);
        timeline.stream(timeline.generation, delta("reply", Some(6)));
        assert!(!timeline.rows[2].active);
        assert_eq!(text(&timeline.rows[3]), "reply");
        let action = timeline.stream(timeline.generation, finish()).remove(0);
        assert!(
            matches!(&action, Action::Fetch { plan: FetchPlan::After(cursor), .. } if cursor.seq == 6)
        );
        assert!(timeline.rows.iter().all(|row| !row.active));
        let mut response = page(&action, "epoch", None, None, vec![]);
        response.window.max_seq = 6;
        response.window.next_seq = 7;
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(timeline.status, Status::Current);
    }

    #[test]
    fn live_tool_lifecycle_does_not_merge_assistant_segments_or_regress_terminal_state() {
        // session-stream-reducers.test.ts:732,950,1933 and stream.ts mergeToolCallDetail.
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, delta("before", Some(2)));
        timeline.stream(
            timeline.generation,
            live(tool(3, 3, "running", &[(3, 3)])["item"].clone(), Some(3)),
        );
        timeline.stream(timeline.generation, delta("after", Some(4)));
        let mut completion = tool(5, 5, "completed", &[(5, 5)])["item"].clone();
        completion["metadata"] = json!({"result":"retained"});
        timeline.stream(timeline.generation, live(completion, Some(5)));
        timeline.stream(timeline.generation, delta(" tool", Some(6)));
        let mut running = tool(7, 7, "running", &[(7, 7)])["item"].clone();
        running["detail"] = json!({"type":"unknown","input":{},"output":null});
        timeline.stream(timeline.generation, live(running, Some(7)));
        assert_eq!(timeline.rows.len(), 4);
        assert_eq!(text(&timeline.rows[1]), "before");
        assert_eq!(text(&timeline.rows[3]), "after tool");
        assert_eq!(
            timeline.rows[3].sources,
            vec![
                SourceRange { start: 4, end: 4 },
                SourceRange { start: 6, end: 6 }
            ]
        );
        assert!(
            matches!(&*timeline.rows[2].item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Completed
            && tool.detail.kind == "read" && tool.metadata.as_ref().unwrap()["result"] == "retained")
        );
        let mut failed = tool(8, 8, "completed", &[(8, 8)])["item"].clone();
        failed["status"] = json!("failed");
        failed["error"] = json!({"message":"failed"});
        timeline.stream(timeline.generation, live(failed, Some(8)));
        assert!(
            matches!(&*timeline.rows[2].item, TimelineItem::Tool(tool) if tool.status == ToolStatus::Failed && !tool.error.is_null())
        );
    }

    #[test]
    fn provisional_text_is_rebased_onto_authoritative_history_without_replaying_its_prefix() {
        // replica.test.ts overlapping projection; session-stream-reducers.test.ts:1964.
        let mut timeline = current(vec![user(1)], 1);
        assert!(
            timeline
                .stream(timeline.generation, delta("Hel", None))
                .is_empty()
        );
        timeline.stream(timeline.generation, delta("lo", None));
        let id = timeline.rows[1].id;
        assert_eq!(text(&timeline.rows[1]), "Hello");
        assert!(timeline.rows[1].provisional);
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 1);
        let action = timeline.catch_up(timeline.generation).remove(0);
        timeline.stream(timeline.generation, delta("!", None));
        let response = page(
            &action,
            "epoch",
            Some(2),
            Some(3),
            vec![assistant(2, 3, "Hello")],
        );
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[1]), "Hello!");
        assert_eq!(timeline.rows[1].id, id);
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 3);
        let action = timeline.stream(timeline.generation, finish()).remove(0);
        let response = page(
            &action,
            "epoch",
            Some(4),
            Some(4),
            vec![assistant(2, 4, "Hello!")],
        );
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[1]), "Hello!");
        assert_eq!(timeline.rows[1].id, id);
        assert!(!timeline.rows[1].provisional);
        assert_eq!(timeline.buffered_events().count(), 0);
    }

    #[test]
    fn committed_updates_during_a_provisional_overlay_and_fetch_are_replayed_in_sequence() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, delta("provisional ", None));
        timeline.stream(timeline.generation, delta("committed ", Some(2)));
        let action = timeline.catch_up(timeline.generation).remove(0);
        assert!(
            matches!(&action, Action::Fetch { plan: FetchPlan::After(cursor), .. } if cursor.seq == 1)
        );
        timeline.stream(timeline.generation, delta("later", Some(3)));
        assert_eq!(text(&timeline.rows[1]), "provisional committed later");
        let response = page(
            &action,
            "epoch",
            Some(2),
            Some(2),
            vec![assistant(2, 2, "committed ")],
        );
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[1]), "committed later");
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 3);
        assert_eq!(timeline.buffered_events().count(), 0);
        assert!(!timeline.rows[1].provisional);
    }

    #[test]
    fn committed_gap_is_parked_until_history_connects_it_and_epoch_change_requires_tail() {
        let mut timeline = current(vec![assistant(1, 2, "AB")], 2);
        let action = timeline
            .stream(timeline.generation, delta("D", Some(4)))
            .remove(0);
        assert_eq!(text(&timeline.rows[0]), "AB");
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 2);
        let response = page(
            &action,
            "epoch",
            Some(3),
            Some(3),
            vec![assistant(1, 3, "ABC")],
        );
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[0]), "ABCD");
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 4);
        let mut next_epoch = delta("new epoch", Some(1));
        next_epoch.epoch = Some("new-epoch".into());
        let action = timeline.stream(timeline.generation, next_epoch).remove(0);
        assert!(matches!(
            action,
            Action::Fetch {
                plan: FetchPlan::Tail,
                ..
            }
        ));
        assert_eq!(text(&timeline.rows[0]), "ABCD");
        let mut response = page(
            &action,
            "new-epoch",
            Some(1),
            Some(1),
            vec![assistant(1, 1, "new epoch")],
        );
        response.reset = true;
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(timeline.rows.len(), 1);
        assert_eq!(text(&timeline.rows[0]), "new epoch");
        assert_eq!(timeline.coverage.as_ref().unwrap().epoch, "new-epoch");
    }

    #[test]
    fn bounded_buffer_requires_resync_and_keeps_displayed_history_on_overflow() {
        let mut timeline = current(vec![user(1)], 1);
        for _ in 0..MAX_BUFFERED_EVENTS {
            assert!(
                timeline
                    .stream(timeline.generation, delta("x", None))
                    .is_empty()
            );
        }
        let rendered = text(&timeline.rows[1]).to_owned();
        let action = timeline
            .stream(timeline.generation, delta("overflow", None))
            .remove(0);
        assert!(matches!(
            action,
            Action::Fetch {
                plan: FetchPlan::Tail,
                ..
            }
        ));
        assert!(timeline.resync_required);
        assert_eq!(timeline.buffered_events().count(), 0);
        assert_eq!(text(&timeline.rows[1]), rendered);
        assert_eq!(timeline.rows.len(), 2);
        assert!(
            timeline
                .stream(timeline.generation, delta("during fetch", None))
                .is_empty()
        );
        let response = page(
            &action,
            "epoch",
            Some(1),
            Some(2),
            vec![user(1), assistant(2, 2, "authoritative")],
        );
        let after = apply(&mut timeline, &action, response).remove(0);
        assert!(timeline.resync_required);
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(text(&timeline.rows[1]), "authoritative");
        let mut response = page(&after, "epoch", None, None, vec![]);
        response.window.max_seq = 2;
        response.window.next_seq = 3;
        assert!(apply(&mut timeline, &after, response).is_empty());
        assert!(!timeline.resync_required);
        assert_eq!(timeline.status, Status::Current);

        let mut timeline = current(vec![user(1)], 1);
        let oversized = "x".repeat(MAX_BUFFERED_BYTES + 1);
        assert!(matches!(
            timeline.stream(timeline.generation, delta(&oversized, None))[0],
            Action::Fetch {
                plan: FetchPlan::Tail,
                ..
            }
        ));
        assert_eq!(timeline.rows.len(), 1);
        assert!(timeline.resync_required);
        assert_eq!(timeline.buffered_bytes, 0);
    }

    #[test]
    fn rpc_failures_refresh_and_turn_completion_respect_operation_and_generation_ownership() {
        let mut timeline = current(vec![user(1)], 1);
        let old = timeline.refresh(timeline.generation).remove(0);
        let newer = timeline.refresh(timeline.generation).remove(0);
        let Action::Fetch {
            operation_id: old_id,
            ..
        } = &old
        else {
            panic!("not a fetch")
        };
        let Action::Fetch { operation_id, .. } = &newer else {
            panic!("not a fetch")
        };
        timeline.history_failed(timeline.generation, old_id, "late error".into());
        assert_eq!(timeline.status, Status::CatchingUp);
        timeline.history_failed(timeline.generation, operation_id, "network error".into());
        assert_eq!(timeline.status, Status::Failed);
        assert_eq!(timeline.rows.len(), 1);
        let retry = timeline.retry().remove(0);
        assert!(timeline.stream(timeline.generation, finish()).is_empty());
        let response = page(&retry, "epoch", Some(1), Some(1), vec![user(1)]);
        let after = apply(&mut timeline, &retry, response).remove(0);
        assert!(matches!(
            after,
            Action::Fetch {
                plan: FetchPlan::After(_),
                ..
            }
        ));
        let old_generation = timeline.generation;
        timeline.detach();
        assert!(timeline.refresh(old_generation).is_empty());
        assert!(timeline.catch_up(old_generation).is_empty());
        timeline.history_failed(old_generation, operation_id, "detached".into());
        assert_eq!(timeline.status, Status::Detached);
    }

    #[test]
    fn live_unknown_timeline_items_are_preserved_and_user_echoes_use_client_identity() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(
            timeline.generation,
            live(json!({"type":"future_item","private":"kept"}), Some(2)),
        );
        assert!(
            matches!(&*timeline.rows[1].item, TimelineItem::Unknown(value) if value["private"] == "kept")
        );
        timeline.stream(timeline.generation, live(json!({"type":"user_message","text":"prompt","messageId":"provider","clientMessageId":"prompt"}), Some(3)));
        assert_eq!(timeline.rows.len(), 2);
        assert!(
            matches!(&*timeline.rows[0].item, TimelineItem::User(user) if user.message_id.as_deref() == Some("provider"))
        );
        assert_eq!(
            timeline.rows[0].sources,
            vec![
                SourceRange { start: 1, end: 1 },
                SourceRange { start: 3, end: 3 }
            ]
        );
    }

    #[test]
    fn an_empty_catch_up_page_cannot_acknowledge_or_discard_unpersisted_live_text() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, delta("still streaming", None));
        let id = timeline.rows[1].id;
        let action = timeline.catch_up(timeline.generation).remove(0);
        let mut response = page(&action, "epoch", None, None, vec![]);
        response.window.max_seq = 1;
        response.window.next_seq = 2;
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[1]), "still streaming");
        assert_eq!(timeline.rows[1].id, id);
        assert!(timeline.rows[1].provisional);
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 1);
        let action = timeline.stream(timeline.generation, finish()).remove(0);
        let mut response = page(&action, "epoch", None, None, vec![]);
        response.window.max_seq = 1;
        response.window.next_seq = 2;
        assert!(apply(&mut timeline, &action, response).is_empty());
        assert_eq!(text(&timeline.rows[1]), "still streaming");
        assert!(!timeline.rows[1].active);
    }

    fn seeded_history() -> Vec<Value> {
        (0..64).flat_map(|index| {
            let user_seq = index * 2 + 1;
            let assistant_seq = user_seq + 1;
            [entry(user_seq, user_seq, json!({"type":"user_message","text":format!("NSH_HISTORY_USER_{index:03}"),"messageId":format!("history-user-{index}")}), &[(user_seq,user_seq)]),
                entry(assistant_seq, assistant_seq, json!({"type":"assistant_message","text":format!("NSH_HISTORY_REPLY_{index:03}"),"messageId":format!("history-assistant-{index}")}), &[(assistant_seq,assistant_seq)])]
        }).collect()
    }

    fn turn_started() -> Stream {
        serde_json::from_value(
            json!({"agentId":"agent","timestamp":"2026-11-06T00:00:00.000Z",
            "event":{"type":"turn_started","provider":"mock"}}),
        )
        .unwrap()
    }

    fn anonymous(text: &str, seq: Option<u64>) -> Stream {
        live(json!({"type":"assistant_message","text":text}), seq)
    }

    #[test]
    fn seeded_history_canonical_user_and_turn_started_create_an_independent_anonymous_head() {
        // Real GTK failure: 128 seeded rows became 129 by appending the new reply to old #063.
        let mut timeline = current(seeded_history(), 128);
        let old_ids: Vec<u64> = timeline.rows.iter().map(|row| row.id).collect();
        timeline.stream(timeline.generation, live(json!({"type":"user_message","text":"Respond with exactly: NSH_CHAT_STREAM_中文","messageId":"new-user","clientMessageId":"new-client"}), Some(129)));
        timeline.stream(timeline.generation, turn_started());
        timeline.stream(timeline.generation, anonymous("NSH_CHAT_", None));
        timeline.stream(timeline.generation, anonymous("STREAM_中文", None));
        assert_eq!(timeline.rows.len(), 130);
        assert_eq!(text(&timeline.rows[127]), "NSH_HISTORY_REPLY_063");
        assert_eq!(text(&timeline.rows[129]), "NSH_CHAT_STREAM_中文");
        assert_eq!(
            timeline.rows[..128]
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            old_ids
        );
        let head_id = timeline.rows[129].id;
        let action = timeline.stream(timeline.generation, finish()).remove(0);
        let canonical = entry(
            130,
            130,
            json!({"type":"assistant_message","text":"NSH_CHAT_STREAM_中文"}),
            &[(130, 130)],
        );
        assert!(
            apply(
                &mut timeline,
                &action,
                page(&action, "epoch", Some(130), Some(130), vec![canonical])
            )
            .is_empty()
        );
        assert_eq!(timeline.rows.len(), 130);
        assert_eq!(timeline.rows[129].id, head_id);
        assert_eq!(text(&timeline.rows[123]), "NSH_HISTORY_REPLY_061");
        assert_eq!(text(&timeline.rows[127]), "NSH_HISTORY_REPLY_063");
        assert_eq!(
            timeline.rows[..128]
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            old_ids
        );
    }

    #[test]
    fn first_anonymous_chunk_does_not_resume_canonical_tail_even_without_turn_metadata() {
        let mut timeline = current(
            vec![entry(
                1,
                1,
                json!({"type":"assistant_message","text":"old reply"}),
                &[(1, 1)],
            )],
            1,
        );
        timeline.stream(timeline.generation, anonymous("new reply", Some(2)));
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(text(&timeline.rows[0]), "old reply");
        assert_eq!(text(&timeline.rows[1]), "new reply");
        timeline.stream(timeline.generation, anonymous(" continued", Some(3)));
        assert_eq!(timeline.rows.len(), 2);
        assert_eq!(text(&timeline.rows[1]), "new reply continued");
    }

    #[test]
    fn response_including_post_request_provisional_prefix_replays_only_its_uncovered_suffix() {
        for (buffered, expected) in [("Hello", "Hello"), ("Hello!", "Hello!")] {
            let mut timeline = current(vec![user(1)], 1);
            let action = timeline.catch_up(timeline.generation).remove(0);
            timeline.stream(timeline.generation, anonymous(buffered, None));
            let id = timeline.rows[1].id;
            let response = page(
                &action,
                "epoch",
                Some(2),
                Some(2),
                vec![entry(
                    2,
                    2,
                    json!({"type":"assistant_message","text":"Hello"}),
                    &[(2, 2)],
                )],
            );
            assert!(apply(&mut timeline, &action, response).is_empty());
            assert_eq!(timeline.rows.len(), 2);
            assert_eq!(text(&timeline.rows[1]), expected);
            assert_eq!(timeline.rows[1].id, id);
        }
    }

    #[test]
    fn bootstrap_race_and_similar_history_prefixes_preserve_identity_and_message_order() {
        let mut timeline = Timeline::new("agent".into());
        let action = start(&mut timeline);
        timeline.stream(timeline.generation, anonymous("NSH_HISTORY_REPLY_06", None));
        let provisional_id = timeline.rows[0].id;
        let mut rows = seeded_history();
        rows.push(entry(129,129,json!({"type":"user_message","text":"new prompt","messageId":"new-user","clientMessageId":"new-client"}),&[(129,129)]));
        rows.push(entry(
            130,
            130,
            json!({"type":"assistant_message","text":"NSH_HISTORY_REPLY_06"}),
            &[(130, 130)],
        ));
        let after = apply(
            &mut timeline,
            &action,
            page(&action, "epoch", Some(1), Some(130), rows),
        )
        .remove(0);
        assert_eq!(timeline.rows.len(), 130);
        assert_eq!(text(&timeline.rows[123]), "NSH_HISTORY_REPLY_061");
        assert_eq!(text(&timeline.rows[127]), "NSH_HISTORY_REPLY_063");
        assert_eq!(timeline.rows[129].id, provisional_id);
        let ids: std::collections::HashSet<_> = timeline.rows.iter().map(|row| row.id).collect();
        assert_eq!(ids.len(), 130);
        let mut response = page(&after, "epoch", None, None, vec![]);
        response.window.max_seq = 130;
        response.window.next_seq = 131;
        assert!(apply(&mut timeline, &after, response).is_empty());
    }

    #[test]
    fn committed_users_and_turn_starts_end_the_head_but_temporary_users_can_interrupt_it() {
        let mut timeline = current(vec![user(1)], 1);
        timeline.stream(timeline.generation, anonymous("first", Some(2)));
        timeline.stream(
            timeline.generation,
            live(
                json!({"type":"user_message","text":"new prompt","clientMessageId":"second"}),
                Some(3),
            ),
        );
        timeline.stream(timeline.generation, anonymous("second", Some(4)));
        assert_eq!(timeline.rows.len(), 4);
        assert_eq!(text(&timeline.rows[1]), "first");
        assert_eq!(text(&timeline.rows[3]), "second");
        timeline.stream(timeline.generation, turn_started());
        timeline.stream(timeline.generation, anonymous("third", Some(5)));
        assert_eq!(timeline.rows.len(), 5);
        assert_eq!(text(&timeline.rows[4]), "third");
        timeline.stream(
            timeline.generation,
            live(
                json!({"type":"user_message","text":"queued prompt","clientMessageId":"queued"}),
                None,
            ),
        );
        timeline.stream(timeline.generation, anonymous(" continued", Some(6)));
        assert_eq!(timeline.rows.len(), 6);
        assert_eq!(text(&timeline.rows[4]), "third continued");
        assert_eq!(text(&timeline.rows[5]), "queued prompt");
    }

    #[test]
    fn equal_user_text_after_a_request_does_not_acknowledge_a_different_client_message() {
        let mut timeline = current(vec![user(1)], 1);
        let action = timeline.catch_up(timeline.generation).remove(0);
        timeline.stream(
            timeline.generation,
            live(
                json!({"type":"user_message","text":"same text","clientMessageId":"local"}),
                None,
            ),
        );
        let remote = entry(
            2,
            2,
            json!({"type":"user_message","text":"same text","clientMessageId":"remote"}),
            &[(2, 2)],
        );
        assert!(
            apply(
                &mut timeline,
                &action,
                page(&action, "epoch", Some(2), Some(2), vec![remote])
            )
            .is_empty()
        );
        assert_eq!(timeline.rows.len(), 3);
        assert!(
            matches!(&*timeline.rows[1].item,TimelineItem::User(user) if user.client_message_id.as_deref() == Some("remote"))
        );
        assert!(
            matches!(&*timeline.rows[2].item,TimelineItem::User(user) if user.client_message_id.as_deref() == Some("local"))
        );
    }

    #[test]
    fn a_projection_can_cover_only_one_of_two_equal_provisional_turn_segments() {
        for request_before_stream in [true, false] {
            let mut timeline = current(vec![user(1)], 1);
            let mut action =
                request_before_stream.then(|| timeline.catch_up(timeline.generation).remove(0));
            timeline.stream(timeline.generation, anonymous("Hello", None));
            let first_id = timeline.rows[1].id;
            timeline.stream(timeline.generation, turn_started());
            timeline.stream(timeline.generation, anonymous("Hello", None));
            let second_id = timeline.rows[2].id;
            assert_ne!(first_id, second_id);
            if action.is_none() {
                action = Some(timeline.catch_up(timeline.generation).remove(0));
            }
            let action = action.unwrap();
            let canonical = entry(
                2,
                2,
                json!({"type":"assistant_message","text":"Hello"}),
                &[(2, 2)],
            );
            assert!(
                apply(
                    &mut timeline,
                    &action,
                    page(&action, "epoch", Some(2), Some(2), vec![canonical])
                )
                .is_empty()
            );
            assert_eq!(timeline.rows.len(), 3);
            assert_eq!(text(&timeline.rows[1]), "Hello");
            assert_eq!(text(&timeline.rows[2]), "Hello");
            assert_eq!(timeline.rows[1].id, first_id);
            assert_eq!(timeline.rows[2].id, second_id);
            assert!(!timeline.rows[1].provisional);
            assert!(timeline.rows[2].provisional);
            assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 2);
        }
    }

    #[test]
    fn after_projection_prefix_before_committed_stream_resumes_one_live_reply() {
        // Real daemon After pages carry whole projected items across the requested cursor.
        let mut timeline = current(seeded_history(), 128);
        let old_ids: Vec<_> = timeline.rows.iter().map(|row| row.id).collect();
        let mut prompt = live(
            json!({"type":"user_message","text":"Respond with exactly: NSH_CHAT_STREAM_中文","clientMessageId":"new-client"}),
            Some(129),
        );
        if let StreamEvent::Known(KnownStreamEvent::Timeline { turn_id, .. }) = &mut prompt.event {
            *turn_id = Some("fake-turn-0".into());
        }
        timeline.stream(timeline.generation, prompt);
        timeline.stream(timeline.generation, turn_started());
        let after = timeline.catch_up(timeline.generation).remove(0);
        let mut prefix = entry(
            130,
            130,
            json!({"type":"assistant_message","text":"NSH_CH"}),
            &[(130, 130)],
        );
        prefix["turnId"] = json!("fake-turn-0");
        assert!(
            apply(
                &mut timeline,
                &after,
                page(&after, "epoch", Some(130), Some(130), vec![prefix])
            )
            .is_empty()
        );
        for (seq, chunk) in [(130, "NSH_CH"), (131, "AT_STREAM_中文")] {
            let mut event = anonymous(chunk, Some(seq));
            if let StreamEvent::Known(KnownStreamEvent::Timeline { turn_id, .. }) = &mut event.event
            {
                *turn_id = Some("fake-turn-0".into());
            }
            assert!(timeline.stream(timeline.generation, event).is_empty());
        }
        assert_eq!(timeline.rows.len(), 130);
        assert_eq!(text(&timeline.rows[129]), "NSH_CHAT_STREAM_中文");
        assert_eq!(text(&timeline.rows[127]), "NSH_HISTORY_REPLY_063");
        assert_eq!(
            timeline.rows[..128]
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            old_ids
        );
        assert_eq!(timeline.coverage.as_ref().unwrap().contiguous.span.end, 131);
        let completion = timeline.stream(timeline.generation, finish()).remove(0);
        let mut response = page(&completion, "epoch", None, None, vec![]);
        response.window.max_seq = 131;
        response.window.next_seq = 132;
        assert!(apply(&mut timeline, &completion, response).is_empty());
        assert_eq!(timeline.rows.len(), 130);
        assert_eq!(text(&timeline.rows[129]), "NSH_CHAT_STREAM_中文");
    }
}
