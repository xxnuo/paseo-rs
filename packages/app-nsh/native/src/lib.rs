//! Rust-first Paseo client. Shared protocol probes live outside the product UI.

mod client;
mod host;
#[path = "ui/host.rs"]
mod host_ui;
mod protocol;

use nsh_core::prelude::*;

/// Registers the Rust application root for every NSH platform.
#[native_module("paseo/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// Connection state and tasks belong to this root, independently of page contents.
#[component]
pub fn App() -> impl IntoElement {
    host_ui::screen()
}
