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
            .shows("地址有效：wss://example.com/private?token=secret")
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
    assert!(!app.screen().shows("地址有效：ws://user:secret@localhost"));
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
    app.wait_for("地址有效（已提供密码）：wss://example.com")
        .unwrap();
}

#[test]
fn connecting_can_be_cancelled_without_waiting_for_the_handshake() {
    use nsh_core::websocket::{
        WebSocketData, WebSocketEvents, WebSocketRequest, WebSocketTask, WebSocketTransport,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Pending(Arc<AtomicUsize>);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl WebSocketTask for Pending {
        fn send(&mut self, _: WebSocketData) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self, _: u16, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn buffered_amount(&self) -> usize {
            0
        }
        fn pause(&mut self, _: bool) {}
    }
    struct Transport(Arc<AtomicUsize>);
    impl WebSocketTransport for Transport {
        fn connect(
            &self,
            _: WebSocketRequest,
            _: WebSocketEvents,
        ) -> Result<Box<dyn WebSocketTask>, String> {
            Ok(Box::new(Pending(self.0.clone())))
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.websocket = Some(Arc::new(Transport(dropped.clone())));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("正在连接主机…").unwrap();
    assert!(app.screen().enabled("role:button[断开连接]").unwrap());
    app.press("role:button[断开连接]").unwrap();
    app.wait_for("已断开连接").unwrap();
    assert!(app.screen().enabled("label:主机地址").unwrap());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
