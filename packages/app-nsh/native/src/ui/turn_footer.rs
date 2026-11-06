//! Turn actions and clocks are isolated from the message list's stream updates.

use crate::state::turn_presentation::{CompletedFooter, Timestamp};
use nsh_base::{button, theme};
use nsh_core::{platform::Lifecycle, prelude::*};
use std::{cell::RefCell, rc::Rc, time::Duration};

pub(super) fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        if seconds % 60 == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {}s", seconds % 60)
        }
    } else {
        let hours = seconds / 3600;
        let minutes = seconds / 60 % 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    }
}

fn timestamp_label(timestamp: Timestamp) -> Option<String> {
    let utc = chrono::DateTime::from_timestamp_millis(timestamp.unix_ms)?;
    #[cfg(not(target_arch = "wasm32"))]
    let local = utc.with_timezone(&chrono::Local).naive_local();
    #[cfg(target_arch = "wasm32")]
    let local = {
        #[link(wasm_import_module = "nsh")]
        unsafe extern "C" {
            fn local_offset(ms: f64) -> f64;
        }
        // SAFETY: NSH exposes this same pure host function to its calendar component.
        let east = unsafe { local_offset(timestamp.unix_ms as f64) };
        if !east.is_finite() {
            return None;
        }
        utc.checked_add_signed(chrono::TimeDelta::try_seconds(east as i64)?)?
            .naive_utc()
    };
    // shortcut: fixed local date format; adopt the shared locale/hour-cycle settings with i18n.
    Some(local.format("%Y-%m-%d %H:%M").to_string())
}

pub(super) fn completed(
    footer: Memo<Option<CompletedFooter>>,
    copy: Rc<dyn Fn(CompletedFooter)>,
) -> impl IntoElement {
    let revealed = signal(false);
    let copied = signal(false);
    let reset = Rc::new(RefCell::new(None::<Spawned<()>>));
    let copy_reset = reset.clone();
    let tap_reset = Rc::new(RefCell::new(None::<Spawned<()>>));
    let reveal_reset = tap_reset.clone();
    cleanup(move || {
        reset.borrow_mut().take();
        tap_reset.borrow_mut().take();
    });
    view()
        .flex_row()
        .gap_2()
        .items_center()
        .child(when(
            move || footer.with(|value| value.as_ref().is_some_and(|value| value.copyable)),
            move || {
                let copy = copy.clone();
                let copy_reset = copy_reset.clone();
                button()
                    .on_press(move || {
                        if let Some(value) = footer.get_untracked() {
                            copy(value);
                            copied.set(true);
                            copy_reset.replace(Some(set_timeout(
                                Duration::from_millis(1500),
                                move || copied.set(false),
                            )));
                        }
                    })
                    .p_1()
                    .child(text(move || {
                        if copied.get() {
                            "已复制"
                        } else {
                            "复制本轮回复"
                        }
                    }))
            },
        ))
        .child(when(
            move || {
                footer.with(|value| {
                    value
                        .as_ref()
                        .and_then(|value| value.timing.as_ref())
                        .is_some()
                })
            },
            move || {
                let reveal_reset = reveal_reset.clone();
                view()
                    .on_mouse_enter(move |_: &MouseEvent| revealed.set(true))
                    .on_mouse_leave(move |_: &MouseEvent| revealed.set(false))
                    .child(
                        button()
                            .on_press(move || {
                                revealed.set(!revealed.get_untracked());
                                reveal_reset.replace(Some(set_timeout(
                                    Duration::from_secs(3),
                                    move || revealed.set(false),
                                )));
                            })
                            .p_1()
                            .child(
                                text(move || {
                                    footer.with(|value| {
                                        let Some(timing) =
                                            value.as_ref().and_then(|value| value.timing.as_ref())
                                        else {
                                            return String::new();
                                        };
                                        if !revealed.get()
                                            && let Some(ms) = timing.duration_ms
                                        {
                                            return format!("用时 {}", duration(ms));
                                        }
                                        timestamp_label(timing.completed_at).unwrap_or_default()
                                    })
                                })
                                .text_size(px(12.0))
                                .text_color(move || theme().muted_foreground),
                            ),
                    )
            },
        ))
}

pub(super) fn running(started_at: Memo<Option<Timestamp>>) -> impl IntoElement {
    let now = signal(nsh_core::time::unix_ms() as i64);
    let ticker = Rc::new(RefCell::new(None::<Spawned<()>>));
    let active_ticker = ticker.clone();
    effect(move || {
        if lifecycle() == Lifecycle::Active {
            now.set(nsh_core::time::unix_ms() as i64);
            active_ticker.replace(Some(set_interval(Duration::from_secs(1), move || {
                now.set(nsh_core::time::unix_ms() as i64)
            })));
        } else {
            active_ticker.borrow_mut().take();
        }
    });
    cleanup(move || {
        ticker.borrow_mut().take();
    });
    text(move || match started_at.get() {
        Some(start) => format!(
            "正在处理 · {}",
            duration(now.get().saturating_sub(start.unix_ms).max(0) as u64)
        ),
        None => "正在处理".into(),
    })
    .test_id("turn-working-indicator")
    .text_size(px(12.0))
    .text_color(move || theme().muted_foreground)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations_match_upstream_seconds_minutes_and_hours_without_rounding_up() {
        for (ms, label) in [
            (0, "0s"),
            (999, "0s"),
            (59_999, "59s"),
            (60_000, "1m"),
            (72_999, "1m 12s"),
            (3_600_000, "1h"),
            (3_912_999, "1h 5m"),
        ] {
            assert_eq!(duration(ms), label);
        }
    }
}
