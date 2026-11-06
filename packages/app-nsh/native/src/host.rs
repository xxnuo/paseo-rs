use url::Url;

const INVALID_HOST: &str = "主机地址无效，请使用 host:port、端口号、ws://、wss:// 或 tcp:// 地址";

/// Matches the direct-address grammar in protocol/src/host-target.ts.
/// The returned transport URL can contain tokens; display host_url_label instead.
pub(crate) fn normalize_host_url(input: &str) -> Result<String, String> {
    normalize(input.trim()).ok_or_else(|| INVALID_HOST.to_owned())
}

fn normalize(input: &str) -> Option<String> {
    if input.starts_with("ws://") || input.starts_with("wss://") {
        let mut url = Url::parse(input).ok()?;
        if !is_credential_free(&url) {
            return None;
        }
        if url.path() == "/" {
            url.set_path("/ws");
        }
        return Some(url.into());
    }

    if input.starts_with("tcp://") {
        let url = Url::parse(input).ok()?;
        if !is_credential_free(&url) || !matches!(url.path(), "" | "/") {
            return None;
        }
        let host = url.host_str()?;
        let port = url.port()?;
        if port == 0 {
            return None;
        }
        let scheme = if url
            .query_pairs()
            .find(|(key, _)| key == "ssl")
            .is_some_and(|(_, value)| value == "true")
        {
            "wss"
        } else {
            "ws"
        };
        return Url::parse(&format!("{scheme}://{host}:{port}/ws"))
            .ok()
            .map(Into::into);
    }

    let endpoint = if !input.is_empty() && input.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("127.0.0.1:{input}")
    } else {
        input.to_owned()
    };
    let (host, port) = endpoint.rsplit_once(':')?;
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if port.parse::<u16>().ok()? == 0 {
        return None;
    }
    let valid_host = if let Some(ipv6) = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        !ipv6.is_empty()
            && ipv6
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'))
    } else {
        host.as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    };
    if !valid_host {
        return None;
    }
    Url::parse(&format!("ws://{host}:{port}/ws"))
        .ok()
        .map(Into::into)
}

fn is_credential_free(url: &Url) -> bool {
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        // Passwords need a separate credential channel, never persisted in the address.
        && !url.query_pairs().any(|(key, _)| key == "password")
}

/// Hide paths and query parameters, which may contain proxy credentials.
pub(crate) fn host_url_label(input: &str) -> String {
    normalize(input.trim())
        .and_then(|normalized| Url::parse(&normalized).ok())
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "主机地址无效".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_direct_addresses_like_the_upstream_connector() {
        for (input, expected) in [
            (" localhost:6767 ", "ws://localhost:6767/ws"),
            ("4321", "ws://127.0.0.1:4321/ws"),
            ("[::1]:4321", "ws://[::1]:4321/ws"),
            ("build-box.example.:6767", "ws://build-box.example.:6767/ws"),
            ("my_alias:6767", "ws://my_alias:6767/ws"),
            ("EXAMPLE.com:00080", "ws://example.com/ws"),
            ("ws://EXAMPLE.com:80", "ws://example.com/ws"),
            ("wss://example.com:443/", "wss://example.com/ws"),
            ("wss://例子.测试", "wss://xn--fsqu00a.xn--0zwm56d/ws"),
            ("ws://[::1]", "ws://[::1]/ws"),
            ("tcp://localhost:6767", "ws://localhost:6767/ws"),
            ("tcp://[::1]:6767?ssl=true", "wss://[::1]:6767/ws"),
            ("tcp://example.com:443/?ssl=true", "wss://example.com/ws"),
        ] {
            assert_eq!(normalize_host_url(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn preserves_websocket_paths_and_proxy_queries() {
        for (input, expected) in [
            (
                "wss://example.com/custom?token=secret",
                "wss://example.com/custom?token=secret",
            ),
            (
                "wss://example.com/?token=secret",
                "wss://example.com/ws?token=secret",
            ),
            ("ws://example.com/a%2Fb/", "ws://example.com/a%2Fb/"),
            ("ws://example.com/a/../custom", "ws://example.com/custom"),
        ] {
            assert_eq!(normalize_host_url(input).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_invalid_or_credential_bearing_addresses_without_echoing_input() {
        for input in [
            "",
            "localhost",
            "0",
            "65536",
            "localhost:0",
            "localhost:65536",
            "localhost:123456",
            "localhost/path:6767",
            "localhost?token=secret:6767",
            "user:secret@localhost:6767",
            "::1:6767",
            "[bad::address]:6767",
            "http://localhost:6767",
            "https://localhost",
            "ssh://localhost",
            "unix:///tmp/daemon",
            "relay://localhost:6767/id?key=secret",
            "ws://user:secret@localhost",
            "wss://user@localhost",
            "ws://localhost?password=secret",
            "ws://localhost?pass%77ord=secret",
            "tcp://localhost:6767?password=secret",
            "tcp://localhost",
            "tcp://localhost:0",
            "tcp://localhost:6767/custom",
            "ws://localhost/#secret",
        ] {
            assert_eq!(normalize_host_url(input).unwrap_err(), INVALID_HOST);
            assert_eq!(host_url_label(input), "主机地址无效");
        }
    }

    #[test]
    fn display_labels_never_include_paths_or_query_credentials() {
        for (input, expected) in [
            (
                "wss://example.com/private-secret?token=secret",
                "wss://example.com",
            ),
            ("ws://localhost:6767/ws?key=secret", "ws://localhost:6767"),
            ("[::1]:6767", "ws://[::1]:6767"),
        ] {
            assert_eq!(host_url_label(input), expected);
        }
    }
}
