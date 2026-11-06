//! Real Rust root input and validation; no JavaScript or fake connection state.

use nsh_engine::{EngineConfig, Root};
use nsh_testing::Headless;

#[test]
fn host_address_validation_clears_when_edited() {
    let mut app = Headless::start(EngineConfig::new(Root::of::<paseo_nsh::App>())).unwrap();
    app.wait_for("连接开发主机").unwrap();
    assert!(!app.screen().enabled("role:button[检查地址]").unwrap());
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[检查地址]").unwrap();
    app.wait_for("地址有效：ws://localhost:6767").unwrap();
    app.type_text("label:主机地址", "/more").unwrap();
    assert!(!app.screen().shows("地址有效：ws://localhost:6767"));
}

#[test]
fn validation_status_does_not_echo_credentials() {
    let mut app = Headless::start(EngineConfig::new(Root::of::<paseo_nsh::App>())).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "wss://example.com/private?token=secret")
        .unwrap();
    app.press("role:button[检查地址]").unwrap();
    app.wait_for("地址有效：wss://example.com").unwrap();
    assert!(
        !app.screen()
            .shows("text:地址有效：wss://example.com/private?token=secret")
    );
}

#[test]
fn invalid_address_shows_a_safe_error() {
    let mut app = Headless::start(EngineConfig::new(Root::of::<paseo_nsh::App>())).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://user:secret@localhost")
        .unwrap();
    app.press("role:button[检查地址]").unwrap();
    app.wait_for("主机地址无效，请使用 host:port、端口号、ws://、wss:// 或 tcp:// 地址")
        .unwrap();
    assert!(
        !app.screen()
            .shows("text:地址有效：ws://user:secret@localhost")
    );
}

#[test]
fn password_address_reports_authentication_without_echoing_the_secret() {
    let mut app = Headless::start(EngineConfig::new(Root::of::<paseo_nsh::App>())).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text(
        "label:主机地址",
        "wss://example.com/private?password=secret&token=proxy",
    )
    .unwrap();
    app.press("role:button[检查地址]").unwrap();
    app.wait_for("地址有效（需要密码认证）：wss://example.com")
        .unwrap();
}
