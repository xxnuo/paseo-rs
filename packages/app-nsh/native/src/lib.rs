//! Rust-first Paseo client. Shared protocol probes live outside the product UI.

mod host;
#[cfg(test)]
mod protocol;

use nsh_base::{button, input, nav_page, nav_stack, theme};
use nsh_core::prelude::*;

/// Registers the Rust application root for every NSH platform.
#[native_module("paseo/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The initial native host-address feasibility surface.
#[component]
pub fn App() -> impl IntoElement {
    let address = signal(String::new());
    let status = signal(String::new());
    let check = move || {
        status.set(match host::parse_direct_host(&address.get_untracked()) {
            Ok(host) => {
                let _transport_url = host.url;
                if host.password.is_some() {
                    format!("地址有效（已提供密码）：{}", host.label)
                } else {
                    format!("地址有效：{}", host.label)
                }
            }
            Err(error) => error,
        });
    };
    nav_stack().flex_1().child(
        nav_page().title("Paseo").child(
            view()
                .flex_1()
                .p_4()
                .gap_3()
                .pb_safe_area_plus(px(16.0))
                .bg(move || theme().background)
                .child(text("连接开发主机").text_color(move || theme().foreground))
                .child(
                    input()
                        .label("主机地址")
                        .placeholder("ws://localhost:6767/ws")
                        .value(move || address.get())
                        .on_change(move |value: &str| {
                            address.set(value.to_owned());
                            status.set(String::new());
                        })
                        .on_submit(move |_: &str| check())
                        .p_3()
                        .border_1()
                        .border_color(move || theme().input)
                        .text_color(move || theme().foreground),
                )
                .child(
                    button()
                        .disabled(move || address.with(|value| value.trim().is_empty()))
                        .on_press(check)
                        .p_3()
                        .child(text("检查地址").text_color(move || theme().primary)),
                )
                .child(text(move || status.get()).text_color(move || theme().muted_foreground)),
        ),
    )
}
