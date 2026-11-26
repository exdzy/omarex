//! Endpoint parsing, normalization, and default local Rex socket discovery.

use std::fmt;
use std::path::PathBuf;

use crate::ProtoError;

/// Normalized Rex server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RexEndpoint {
    /// Local Unix domain socket (`unix:///run/user/1000/rex/server.sock`).
    Unix(PathBuf),
    /// TCP / HTTP / HTTPS / Tailscale WebSocket endpoint.
    Tcp {
        host: String,
        port: u16,
        tls: bool,
        path_prefix: String,
    },
    /// SSH tunnel endpoint (`ssh://[user@]host[:port]`) dialed via `rex __internal dial-stdio`.
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
    },
}

impl RexEndpoint {
    /// Discover the default local Rex server endpoint.
    ///
    /// Order matches `/usr/bin/rex`:
    /// 1. `REX_SERVER` environment variable (if non-empty and valid)
    /// 2. `$XDG_RUNTIME_DIR/rex/server.sock`
    /// 3. `/run/user/<uid>/rex/server.sock` (detected from `/proc/self/status`)
    /// 4. `/tmp/rex/server.sock`
    pub fn discover_default() -> Self {
        if let Ok(raw) = std::env::var("REX_SERVER") {
            let trimmed = raw.trim();
            if !trimmed.is_empty()
                && let Ok(ep) = Self::parse(trimmed)
            {
                return ep;
            }
        }

        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let trimmed = runtime_dir.trim();
            if !trimmed.is_empty() {
                return Self::Unix(PathBuf::from(trimmed).join("rex").join("server.sock"));
            }
        }

        if let Some(uid) = current_linux_uid() {
            return Self::Unix(PathBuf::from(format!("/run/user/{uid}/rex/server.sock")));
        }

        Self::Unix(PathBuf::from("/tmp/rex/server.sock"))
    }

    /// Parse and normalize an endpoint string.
    ///
    /// Supported forms:
    /// - `unix:///path/to/server.sock` or absolute `/path/to/server.sock`
    /// - `https://host[:port][/prefix]` and `wss://host[:port][/prefix]`
    /// - `http://host[:port][/prefix]` and `ws://host[:port][/prefix]`
    /// - `tailnet://node-or-fqdn[:port]` (normalized to HTTPS)
    /// - `ssh://[user@]host[:port]`
    /// - `host:port` (plain HTTP unless port is 443)
    /// - bare hostname (e.g. `studio.tail1234.ts.net` -> HTTPS on port 443)
    pub fn parse(raw: &str) -> Result<Self, ProtoError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(ProtoError::Endpoint("empty endpoint".into()));
        }

        if let Some(rest) = s.strip_prefix("unix://") {
            if rest.is_empty() {
                return Err(ProtoError::Endpoint(
                    "unix:// endpoint requires a socket path".into(),
                ));
            }
            return Ok(Self::Unix(PathBuf::from(rest)));
        }

        if s.starts_with('/') {
            return Ok(Self::Unix(PathBuf::from(s)));
        }

        if let Some(rest) = s.strip_prefix("ssh://") {
            return parse_ssh_endpoint(rest);
        }

        if let Some(rest) = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("wss://"))
            .or_else(|| s.strip_prefix("tailnet://"))
        {
            return parse_tcp_endpoint(rest, true, 443);
        }

        if let Some(rest) = s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("ws://"))
        {
            return parse_tcp_endpoint(rest, false, 80);
        }

        if s.contains("://") {
            return Err(ProtoError::Endpoint(format!(
                "unsupported endpoint scheme in `{s}`"
            )));
        }

        // Bare host or host:port (e.g., `studio.tail1234.ts.net` or `127.0.0.1:7297`).
        let (host_part, path_prefix) = split_authority_path(s);
        let (host, port_opt) = split_host_port(host_part)?;
        match port_opt {
            Some(port) => Ok(Self::Tcp {
                host,
                port,
                tls: port == 443,
                path_prefix,
            }),
            None => Ok(Self::Tcp {
                host,
                port: 443,
                tls: true,
                path_prefix,
            }),
        }
    }

    /// Build the WebSocket request URI for `/api/control` or `/api/data`.
    pub fn ws_uri(&self, api_path: &str) -> String {
        let path = if api_path.starts_with('/') {
            api_path
        } else {
            "/api/control"
        };
        match self {
            Self::Unix(_) | Self::Ssh { .. } => format!("ws://localhost{path}"),
            Self::Tcp {
                host,
                port,
                tls,
                path_prefix,
            } => {
                let scheme = if *tls { "wss" } else { "ws" };
                let default_port = if *tls { 443 } else { 80 };
                let formatted_host = format_host_for_uri(host);
                let prefix = path_prefix.trim_end_matches('/');
                if *port == default_port {
                    format!("{scheme}://{formatted_host}{prefix}{path}")
                } else {
                    format!("{scheme}://{formatted_host}:{port}{prefix}{path}")
                }
            }
        }
    }

    /// Returns a clean human-readable label for UI display and logging.
    pub fn display_label(&self) -> String {
        match self {
            Self::Unix(p) => format!("unix://{}", p.display()),
            Self::Tcp {
                host,
                port,
                tls,
                path_prefix,
            } => {
                let scheme = if *tls { "https" } else { "http" };
                let default_port = if *tls { 443 } else { 80 };
                let formatted_host = format_host_for_uri(host);
                if *port == default_port {
                    format!("{scheme}://{formatted_host}{path_prefix}")
                } else {
                    format!("{scheme}://{formatted_host}:{port}{path_prefix}")
                }
            }
            Self::Ssh { user, host, port } => {
                let mut out = String::from("ssh://");
                if let Some(u) = user {
                    out.push_str(u);
                    out.push('@');
                }
                out.push_str(&format_host_for_uri(host));
                if let Some(p) = port {
                    out.push(':');
                    out.push_str(&p.to_string());
                }
                out
            }
        }
    }

    /// Whether this endpoint connects to a local Unix socket.
    pub fn is_local_unix(&self) -> bool {
        matches!(self, Self::Unix(_))
    }
}

impl fmt::Display for RexEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_label())
    }
}

fn parse_tcp_endpoint(rest: &str, tls: bool, default_port: u16) -> Result<RexEndpoint, ProtoError> {
    let (authority, path_prefix) = split_authority_path(rest);
    if authority.is_empty() {
        return Err(ProtoError::Endpoint("missing host in endpoint".into()));
    }
    let (host, port_opt) = split_host_port(authority)?;
    Ok(RexEndpoint::Tcp {
        host,
        port: port_opt.unwrap_or(default_port),
        tls,
        path_prefix,
    })
}

fn parse_ssh_endpoint(rest: &str) -> Result<RexEndpoint, ProtoError> {
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err(ProtoError::Endpoint(
            "ssh:// endpoint requires a host".into(),
        ));
    }
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((u, hp)) if !u.is_empty() => (Some(u.to_string()), hp),
        Some((_, _)) => {
            return Err(ProtoError::Endpoint(
                "empty username before '@' in ssh:// endpoint".into(),
            ));
        }
        None => (None, authority),
    };
    let (host, port) = split_host_port(host_port)?;
    Ok(RexEndpoint::Ssh { user, host, port })
}

fn split_authority_path(s: &str) -> (&str, String) {
    match s.find('/') {
        Some(idx) => {
            let prefix = s[idx..].trim_end_matches('/').to_string();
            (&s[..idx], prefix)
        }
        None => (s, String::new()),
    }
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>), ProtoError> {
    if let Some(after_open) = authority.strip_prefix('[') {
        let Some(close_idx) = after_open.find(']') else {
            return Err(ProtoError::Endpoint(format!(
                "unterminated IPv6 host in `{authority}`"
            )));
        };
        let host = after_open[..close_idx].to_string();
        if host.is_empty() {
            return Err(ProtoError::Endpoint("empty IPv6 host".into()));
        }
        let remainder = &after_open[close_idx + 1..];
        if remainder.is_empty() {
            return Ok((host, None));
        }
        let Some(port_str) = remainder.strip_prefix(':') else {
            return Err(ProtoError::Endpoint(format!(
                "unexpected characters after IPv6 host in `{authority}`"
            )));
        };
        let port = port_str
            .parse::<u16>()
            .map_err(|_| ProtoError::Endpoint(format!("invalid port `{port_str}`")))?;
        return Ok((host, Some(port)));
    }

    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if host.is_empty() {
            return Err(ProtoError::Endpoint(format!(
                "missing host before port in `{authority}`"
            )));
        }
        if !host.contains(':') {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| ProtoError::Endpoint(format!("invalid port `{port_str}`")))?;
            return Ok((host.to_string(), Some(port)));
        }
    }

    if authority.is_empty() {
        return Err(ProtoError::Endpoint("empty host".into()));
    }
    Ok((authority.to_string(), None))
}

fn format_host_for_uri(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn current_linux_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let first = rest.split_whitespace().next()?;
            return first.parse::<u32>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_endpoint_forms() {
        assert_eq!(
            RexEndpoint::parse("unix:///run/user/1000/rex/server.sock").unwrap(),
            RexEndpoint::Unix(PathBuf::from("/run/user/1000/rex/server.sock"))
        );
        assert_eq!(
            RexEndpoint::parse("/run/user/1000/rex/server.sock").unwrap(),
            RexEndpoint::Unix(PathBuf::from("/run/user/1000/rex/server.sock"))
        );
        assert_eq!(
            RexEndpoint::parse("studio.tail1234.ts.net").unwrap(),
            RexEndpoint::Tcp {
                host: "studio.tail1234.ts.net".into(),
                port: 443,
                tls: true,
                path_prefix: String::new(),
            }
        );
        assert_eq!(
            RexEndpoint::parse("https://studio.tail000000.ts.net/").unwrap(),
            RexEndpoint::Tcp {
                host: "studio.tail000000.ts.net".into(),
                port: 443,
                tls: true,
                path_prefix: String::new(),
            }
        );
        assert_eq!(
            RexEndpoint::parse("http://127.0.0.1:7297").unwrap(),
            RexEndpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 7297,
                tls: false,
                path_prefix: String::new(),
            }
        );
        assert_eq!(
            RexEndpoint::parse("192.168.1.20:7297").unwrap(),
            RexEndpoint::Tcp {
                host: "192.168.1.20".into(),
                port: 7297,
                tls: false,
                path_prefix: String::new(),
            }
        );
        assert_eq!(
            RexEndpoint::parse("ssh://alice@devbox.local:2222").unwrap(),
            RexEndpoint::Ssh {
                user: Some("alice".into()),
                host: "devbox.local".into(),
                port: Some(2222),
            }
        );
        assert_eq!(
            RexEndpoint::parse("ssh://devbox").unwrap(),
            RexEndpoint::Ssh {
                user: None,
                host: "devbox".into(),
                port: None,
            }
        );
    }

    #[test]
    fn builds_ws_uris_correctly() {
        let unix = RexEndpoint::parse("unix:///run/user/1000/rex/server.sock").unwrap();
        assert_eq!(unix.ws_uri("/api/control"), "ws://localhost/api/control");

        let ts = RexEndpoint::parse("studio.tail1234.ts.net").unwrap();
        assert_eq!(
            ts.ws_uri("/api/data"),
            "wss://studio.tail1234.ts.net/api/data"
        );

        let tcp = RexEndpoint::parse("http://[::1]:7297/rex").unwrap();
        assert_eq!(
            tcp.ws_uri("/api/control"),
            "ws://[::1]:7297/rex/api/control"
        );
    }

    #[test]
    fn rejects_invalid_endpoints() {
        assert!(RexEndpoint::parse("").is_err());
        assert!(RexEndpoint::parse("unix://").is_err());
        assert!(RexEndpoint::parse("ftp://example.com").is_err());
        assert!(RexEndpoint::parse("ssh://@host").is_err());
        assert!(RexEndpoint::parse("http://host:notaport").is_err());
    }
}
