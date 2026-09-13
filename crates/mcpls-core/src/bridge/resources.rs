//! MCP resource URI codec and subscription tracking for LSP diagnostics.
//!
//! Resources in mcpls use the `lsp-diagnostics:///` scheme (RFC 3986 compliant,
//! empty authority, percent-encoded path). Each resource corresponds to a single
//! file whose diagnostics are cached from LSP `textDocument/publishDiagnostics`
//! notifications.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rmcp::{Peer, RoleServer};
use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use super::ConnectionId;
use super::state::encode_rfc3986_path_chars;

/// URI scheme used for diagnostic resources.
const SCHEME: &str = "lsp-diagnostics";

/// Full scheme + authority prefix (`scheme://`).
///
/// Three-slash form (`lsp-diagnostics:///`) is produced by appending an empty
/// authority and the absolute path: `{PREFIX}{path}`.
const PREFIX: &str = "lsp-diagnostics://";

/// Maximum number of resource URIs a single client session may subscribe to.
///
/// Guards against memory exhaustion from a misbehaving or adversarial client.
pub const MAX_SUBSCRIPTIONS: usize = 1_000;

/// Errors produced by the resource URI codec.
#[derive(Debug, Error)]
pub enum ResourceUriError {
    /// The path is relative or contains non-UTF-8 components.
    #[error("path must be absolute and valid UTF-8: {0}")]
    InvalidPath(String),

    /// The URI has the wrong scheme or malformed structure.
    #[error("expected '{SCHEME}:///' prefix in URI: {0}")]
    InvalidScheme(String),

    /// The URI path could not be decoded to a filesystem path.
    #[error("failed to decode URI to filesystem path: {0}")]
    DecodeFailed(String),
}

/// Encode an absolute filesystem path into a `lsp-diagnostics:///…` resource URI.
///
/// Percent-encoding is delegated to [`url::Url::from_file_path`], which
/// handles spaces, unicode, `%`, `?`, `#`, and platform separators correctly,
/// plus an additional pass for the RFC 3986 §2.2 "other reserved" characters
/// (`[ ] ^ |`) that `url` otherwise leaves unescaped — the same encoding
/// applied to `file://` URIs.
///
/// # Errors
///
/// Returns [`ResourceUriError::InvalidPath`] if the path is relative or
/// cannot be expressed as a valid file URI.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::resources::make_uri;
///
/// let uri = make_uri(&std::env::temp_dir().join("main.rs")).unwrap();
/// assert!(uri.starts_with("lsp-diagnostics:///"));
/// ```
pub fn make_uri(path: &Path) -> Result<String, ResourceUriError> {
    let file_url = Url::from_file_path(path)
        .map_err(|()| ResourceUriError::InvalidPath(path.display().to_string()))?;

    // Replace the "file" scheme with our custom scheme while keeping the
    // percent-encoded path and authority (empty) components.
    let encoded = encode_rfc3986_path_chars(&file_url);
    let after_scheme = encoded.strip_prefix(file_url.scheme()).unwrap_or(&encoded);
    let uri = format!("{SCHEME}{after_scheme}");
    Ok(uri)
}

/// Decode a `lsp-diagnostics:///…` resource URI back to an absolute filesystem path.
///
/// # Errors
///
/// Returns an error if the URI does not start with the expected scheme,
/// or if the percent-encoded path cannot be mapped to a filesystem path.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::resources::{make_uri, parse_uri};
///
/// let path = std::env::temp_dir().join("main.rs");
/// let uri = make_uri(&path).unwrap();
/// let recovered = parse_uri(&uri).unwrap();
/// assert_eq!(recovered, path);
/// ```
pub fn parse_uri(uri: &str) -> Result<PathBuf, ResourceUriError> {
    if !uri.starts_with(PREFIX) {
        return Err(ResourceUriError::InvalidScheme(uri.to_string()));
    }

    // Require empty authority: the character immediately after `://` must be `/`.
    // This blocks `lsp-diagnostics://evil-host/path` → UNC path on Windows.
    let after_prefix = &uri[PREFIX.len()..];
    if !after_prefix.starts_with('/') {
        return Err(ResourceUriError::InvalidScheme(format!(
            "non-empty authority in URI: {uri}"
        )));
    }

    let file_uri = format!("file://{after_prefix}");
    let url = Url::parse(&file_uri).map_err(|e| ResourceUriError::DecodeFailed(e.to_string()))?;

    url.to_file_path()
        .map_err(|()| ResourceUriError::DecodeFailed(file_uri))
}

/// Which connections want updates for which resource URIs, and the peer
/// each one is notified through.
///
/// One process serves many connections, so a notification for a file goes
/// to the connections subscribed to that file and to no other. The hot read
/// path (the diagnostics pump) takes a read lock, so concurrent readers do
/// not block each other.
#[derive(Default)]
pub struct ResourceSubscriptions(RwLock<HashMap<ConnectionId, Subscriber>>);

struct Subscriber {
    peer: Peer<RoleServer>,
    uris: HashSet<String>,
}

impl std::fmt::Debug for ResourceSubscriptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceSubscriptions")
            .finish_non_exhaustive()
    }
}

impl ResourceSubscriptions {
    /// No subscriptions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `connection`, notified through `peer`, wants updates for
    /// `uri`.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present.
    ///
    /// # Errors
    ///
    /// Returns an error string when `connection` already holds
    /// [`MAX_SUBSCRIPTIONS`] URIs.
    pub async fn subscribe(
        &self,
        connection: ConnectionId,
        peer: Peer<RoleServer>,
        uri: String,
    ) -> Result<bool, String> {
        let mut map = self.0.write().await;
        let subscriber = map.entry(connection).or_insert_with(|| Subscriber {
            peer: peer.clone(),
            uris: HashSet::new(),
        });
        subscriber.peer = peer;
        if !subscriber.uris.contains(&uri) && subscriber.uris.len() >= MAX_SUBSCRIPTIONS {
            return Err(format!("subscription limit of {MAX_SUBSCRIPTIONS} reached"));
        }
        let inserted = subscriber.uris.insert(uri);
        drop(map);
        Ok(inserted)
    }

    /// Whether no connection is subscribed to anything, so the pump can
    /// skip building a URI.
    pub async fn is_empty(&self) -> bool {
        self.0.read().await.is_empty()
    }

    /// Remove `uri` from `connection`'s subscriptions. Returns `true` if it
    /// was present.
    pub async fn unsubscribe(&self, connection: ConnectionId, uri: &str) -> bool {
        let mut map = self.0.write().await;
        let Some(subscriber) = map.get_mut(&connection) else {
            return false;
        };
        let removed = subscriber.uris.remove(uri);
        if subscriber.uris.is_empty() {
            map.remove(&connection);
        }
        removed
    }

    /// Whether `connection` is subscribed to `uri`.
    pub async fn contains(&self, connection: ConnectionId, uri: &str) -> bool {
        self.0
            .read()
            .await
            .get(&connection)
            .is_some_and(|subscriber| subscriber.uris.contains(uri))
    }

    /// Every connection subscribed to `uri`, with the peer to notify.
    pub async fn subscribers(&self, uri: &str) -> Vec<(ConnectionId, Peer<RoleServer>)> {
        self.0
            .read()
            .await
            .iter()
            .filter(|(_, subscriber)| subscriber.uris.contains(uri))
            .map(|(connection, subscriber)| (*connection, subscriber.peer.clone()))
            .collect()
    }

    /// Forget everything `connection` subscribed to: its service stopped,
    /// or a notification to it failed.
    pub async fn remove_connection(&self, connection: ConnectionId) {
        self.0.write().await.remove(&connection);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // URI codec
    // ------------------------------------------------------------------

    #[test]
    fn test_make_uri_rejects_relative_path() {
        let result = make_uri(Path::new("relative/path.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uri_rejects_wrong_scheme() {
        let result = parse_uri("file:///home/user/main.rs");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uri_rejects_http_scheme() {
        let result = parse_uri("https://example.com/file.rs");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_make_uri_simple_path() {
        let uri = make_uri(Path::new("/home/user/main.rs")).unwrap();
        assert_eq!(uri, "lsp-diagnostics:///home/user/main.rs");
    }

    #[cfg(unix)]
    #[test]
    fn test_make_uri_scheme_prefix() {
        let uri = make_uri(Path::new("/tmp/file.rs")).unwrap();
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_uri_simple() {
        let path = PathBuf::from("/home/user/main.rs");
        let uri = make_uri(&path).unwrap();
        let recovered = parse_uri(&uri).unwrap();
        assert_eq!(recovered, path);
    }

    /// Round-trip: paths with spaces, unicode, `%`, `?`, `#`.
    #[cfg(unix)]
    #[test]
    fn test_round_trip_special_chars() {
        let paths = [
            "/home/user/my file.rs",
            "/tmp/café/main.rs",
            "/data/100%/test.rs",
            "/workspace/query?param/file.rs",
            "/repo/branch#fragment/src.rs",
            "/путь/к/файлу.rs",
        ];

        for raw in &paths {
            let path = PathBuf::from(raw);
            let uri = make_uri(&path).expect(raw);
            assert!(
                uri.starts_with("lsp-diagnostics:///"),
                "URI should start with correct scheme: {uri}"
            );
            let recovered = parse_uri(&uri).expect(&uri);
            assert_eq!(recovered, path, "Round-trip failed for: {raw}");
        }
    }

    /// Snapshot test: verify the on-wire form uses three slashes and percent-encoding.
    #[cfg(unix)]
    #[test]
    fn test_wire_format_percent_encoded() {
        let path = Path::new("/home/user/my file.rs");
        let uri = make_uri(path).unwrap();
        // Space must be percent-encoded as %20
        assert!(uri.contains("%20"), "Expected %20 in: {uri}");
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    /// #265 regression: all seven RFC 3986 §2.2 "other reserved" characters
    /// must be percent-encoded in `lsp-diagnostics://` URIs, same as
    /// `file://` URIs from `try_path_to_uri` (see
    /// `test_path_to_uri_percent_encodes_all_rfc3986_other_reserved_chars`
    /// in `state.rs`). `{`, `}`, and backtick are already encoded by the
    /// `url` crate on serialization; `[`, `]`, `^`, `|` are handled
    /// explicitly by `encode_rfc3986_path_chars`.
    #[cfg(unix)]
    #[test]
    fn test_make_uri_percent_encodes_reserved_chars() {
        let path = Path::new("/home/user/test[]^|{}`.ts");
        let uri = make_uri(path).unwrap();

        for (raw, encoded) in [
            ('[', "%5B"),
            (']', "%5D"),
            ('^', "%5E"),
            ('|', "%7C"),
            ('{', "%7B"),
            ('}', "%7D"),
            ('`', "%60"),
        ] {
            assert!(
                uri.contains(encoded),
                "expected {raw:?} to be percent-encoded as {encoded} in {uri}"
            );
        }
        assert!(
            !uri.contains(['[', ']', '^', '|', '{', '}', '`']),
            "no raw reserved characters should remain in {uri}"
        );
        assert_eq!(parse_uri(&uri).unwrap(), path);
    }

    // ------------------------------------------------------------------
    // ResourceSubscriptions
    // ------------------------------------------------------------------

    #[derive(Debug)]
    struct BarePeerHandler;

    impl rmcp::ServerHandler for BarePeerHandler {}

    fn peer() -> Peer<RoleServer> {
        let (server_io, client_io) = tokio::io::duplex(1024);
        let running = rmcp::service::serve_directly(BarePeerHandler, server_io, None);
        let peer = running.peer().clone();
        std::mem::forget((running, client_io));
        peer
    }

    #[tokio::test]
    async fn test_subscribe_and_contains() {
        let subs = ResourceSubscriptions::new();
        let connection = ConnectionId::next();
        let uri = "lsp-diagnostics:///home/user/main.rs".to_string();
        assert!(!subs.contains(connection, &uri).await);
        assert!(
            subs.subscribe(connection, peer(), uri.clone())
                .await
                .unwrap()
        );
        assert!(subs.contains(connection, &uri).await);
        assert!(!subs.contains(ConnectionId::next(), &uri).await);
    }

    #[tokio::test]
    async fn test_subscribe_duplicate_returns_false() {
        let subs = ResourceSubscriptions::new();
        let connection = ConnectionId::next();
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        assert!(
            subs.subscribe(connection, peer(), uri.clone())
                .await
                .unwrap()
        );
        assert!(!subs.subscribe(connection, peer(), uri).await.unwrap());
    }

    #[tokio::test]
    async fn test_unsubscribe_removes_only_that_connections_entry() {
        let subs = ResourceSubscriptions::new();
        let (one, two) = (ConnectionId::next(), ConnectionId::next());
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        subs.subscribe(one, peer(), uri.clone()).await.unwrap();
        subs.subscribe(two, peer(), uri.clone()).await.unwrap();
        assert!(subs.unsubscribe(one, &uri).await);
        assert!(!subs.contains(one, &uri).await);
        assert!(subs.contains(two, &uri).await);
        assert!(
            !subs
                .unsubscribe(one, "lsp-diagnostics:///nonexistent.rs")
                .await
        );
    }

    #[tokio::test]
    async fn test_the_cap_is_per_connection() {
        let subs = ResourceSubscriptions::new();
        let full = ConnectionId::next();
        for i in 0..MAX_SUBSCRIPTIONS {
            subs.subscribe(full, peer(), format!("lsp-diagnostics:///file{i}.rs"))
                .await
                .unwrap();
        }
        assert!(
            subs.subscribe(full, peer(), "lsp-diagnostics:///overflow.rs".to_string())
                .await
                .is_err()
        );
        assert!(
            subs.subscribe(
                ConnectionId::next(),
                peer(),
                "lsp-diagnostics:///overflow.rs".to_string()
            )
            .await
            .is_ok(),
            "one connection's subscriptions must not use up another's"
        );
    }

    #[tokio::test]
    async fn test_subscribers_and_remove_connection() {
        let subs = ResourceSubscriptions::new();
        let (one, two) = (ConnectionId::next(), ConnectionId::next());
        let uri = "lsp-diagnostics:///a.rs".to_string();
        subs.subscribe(one, peer(), uri.clone()).await.unwrap();
        subs.subscribe(two, peer(), uri.clone()).await.unwrap();
        let mut found: Vec<_> = subs
            .subscribers(&uri)
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        found.sort();
        assert_eq!(found, vec![one, two]);
        subs.remove_connection(one).await;
        assert_eq!(subs.subscribers(&uri).await.len(), 1);
        assert!(!subs.is_empty().await);
        subs.remove_connection(two).await;
        assert!(subs.is_empty().await);
    }
}
