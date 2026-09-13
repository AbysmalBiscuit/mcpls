//! The line every endpoint connection opens with.
//!
//! The format is frozen. A build that cannot speak another build's protocol
//! still reads this line and answers it, which is what lets a newer
//! frontend ask an idle older backend to exit and lets `mcpls hook doctor`
//! name the build it reached. Fields are only added, each optional or
//! defaulted, and no reader rejects a field or a refusal it does not know.
//!
//! Both sides read one byte at a time up to the newline, so neither ever
//! buffers bytes of the protocol that follows the handshake.

use std::cmp::Ordering;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::config::{ConfigSource, ServerConfig};

/// The endpoint protocol this build speaks. Raised whenever what follows
/// the handshake changes shape.
pub const PROTOCOL: u32 = 1;

/// This build's version, as the handshake reports it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The longest handshake line either side reads.
const MAX_LINE: usize = 64 * 1024;

/// How long either side waits for the other's handshake line.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// What a connection carries after its handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    /// One host session's MCP traffic, relayed by its frontend.
    Mcp,
    /// Newline-delimited hook requests.
    Hook,
    /// A request that the backend exit, honoured only with no session
    /// attached.
    Shutdown,
}

/// The configuration a process loaded, reduced to what another process
/// compares against its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigStamp {
    /// [`ServerConfig::fingerprint`].
    pub fingerprint: String,
    /// Where the configuration came from.
    pub source: ConfigSource,
    /// Whether a project-local `mcpls.toml` was ignored for want of trust.
    #[serde(default)]
    pub project_ignored: bool,
}

impl ConfigStamp {
    /// The stamp of `config`.
    #[must_use]
    pub fn of(config: &ServerConfig) -> Self {
        Self {
            fingerprint: config.fingerprint(),
            source: config.source,
            project_ignored: config.project_config_ignored,
        }
    }

    /// Whether one side loaded the project's `mcpls.toml` while the other
    /// ignored it as untrusted. Project config can name the command mcpls
    /// spawns, so the two may not share a backend.
    #[must_use]
    pub fn conflicts_with(&self, other: &Self) -> bool {
        (self.source == ConfigSource::Project && other.project_ignored)
            || (other.source == ConfigSource::Project && self.project_ignored)
    }
}

/// The first line a client writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    /// The client's [`PROTOCOL`].
    pub mcpls: u32,
    /// The client's [`VERSION`].
    pub version: String,
    /// What follows.
    pub kind: ConnectionKind,
    /// The canonical checkout root the client resolved.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The session the host named.
    #[serde(default)]
    pub session: Option<String>,
    /// The client's configuration.
    #[serde(default)]
    pub config: Option<ConfigStamp>,
}

impl Handshake {
    /// A frontend attaching one host session.
    #[must_use]
    pub fn mcp(root: PathBuf, session: Option<String>, config: ConfigStamp) -> Self {
        Self {
            root: Some(root),
            session,
            config: Some(config),
            ..Self::bare(ConnectionKind::Mcp)
        }
    }

    /// A hook invocation or `mcpls hook doctor`.
    #[must_use]
    pub fn hook() -> Self {
        Self::bare(ConnectionKind::Hook)
    }

    /// A frontend asking an idle backend to exit.
    #[must_use]
    pub fn shutdown() -> Self {
        Self::bare(ConnectionKind::Shutdown)
    }

    fn bare(kind: ConnectionKind) -> Self {
        Self {
            mcpls: PROTOCOL,
            version: VERSION.to_string(),
            kind,
            root: None,
            session: None,
            config: None,
        }
    }

    /// Whether this handshake came from a build identical to this one.
    #[must_use]
    pub fn same_build(&self) -> bool {
        self.mcpls == PROTOCOL && self.version == VERSION
    }
}

/// The line a server answers a handshake with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeReply {
    /// The server's [`PROTOCOL`].
    pub mcpls: u32,
    /// The server's [`VERSION`].
    pub version: String,
    /// The server's process id.
    pub pid: u32,
    /// How many MCP sessions are attached to the server.
    #[serde(default)]
    pub sessions: usize,
    /// Why the connection is refused, absent when it is accepted.
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

impl HandshakeReply {
    /// This process's answer.
    #[must_use]
    pub fn new(sessions: usize, refusal: Option<Refusal>) -> Self {
        Self {
            mcpls: PROTOCOL,
            version: VERSION.to_string(),
            pid: std::process::id(),
            sessions,
            refusal,
        }
    }

    /// Whether the answering server is a build identical to this one.
    #[must_use]
    pub fn same_build(&self) -> bool {
        self.mcpls == PROTOCOL && self.version == VERSION
    }
}

/// Why a server refused a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The two builds differ.
    Build,
    /// One side trusted the project's `mcpls.toml` and the other did not.
    Trust {
        /// The server's configuration.
        backend: ConfigStamp,
    },
    /// A shutdown was asked for while sessions are attached.
    Attached,
    /// The endpoint is held by an mcpls serving one session in-process.
    InProcess,
    /// The server's configuration turns hooks off.
    HooksDisabled,
    /// A reason this build does not know.
    #[serde(other)]
    Other,
}

/// How two builds order, oldest first: by protocol, then by version,
/// compared numerically component by component.
#[must_use]
pub fn compare_builds(ours: (u32, &str), theirs: (u32, &str)) -> Ordering {
    ours.0
        .cmp(&theirs.0)
        .then_with(|| numeric(ours.1).cmp(&numeric(theirs.1)))
}

fn numeric(version: &str) -> Vec<u64> {
    version
        .split(['.', '-', '+'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Write `value` as one line.
pub(crate) async fn write<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + Sync,
{
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Read one line, one byte at a time, and parse it.
pub(crate) async fn read<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut line = Vec::with_capacity(256);
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if line.len() == MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the handshake line is too long",
            ));
        }
        line.push(byte);
    }
    serde_json::from_slice(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The frozen fields, spelled out. Changing this literal changes what
    /// every older and newer build can read.
    #[test]
    fn test_the_request_line_is_frozen() {
        let literal = r#"{"mcpls":1,"version":"0.3.9","kind":"mcp","root":"/work","session":"s1","config":{"fingerprint":"00000000000000ff","source":"project","project_ignored":false}}"#;
        let parsed: Handshake = serde_json::from_str(literal).unwrap();
        assert_eq!(parsed.mcpls, 1);
        assert_eq!(parsed.kind, ConnectionKind::Mcp);
        assert_eq!(parsed.session.as_deref(), Some("s1"));
        assert_eq!(parsed.config.unwrap().source, ConfigSource::Project);
    }

    #[test]
    fn test_the_reply_line_is_frozen() {
        let literal =
            r#"{"mcpls":1,"version":"0.3.9","pid":42,"sessions":2,"refusal":{"reason":"build"}}"#;
        let parsed: HandshakeReply = serde_json::from_str(literal).unwrap();
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.sessions, 2);
        assert_eq!(parsed.refusal, Some(Refusal::Build));
    }

    #[test]
    fn test_a_newer_builds_lines_still_parse() {
        let request: Handshake = serde_json::from_str(
            r#"{"mcpls":9,"version":"9.0.0","kind":"shutdown","future":true}"#,
        )
        .unwrap();
        assert_eq!(request.kind, ConnectionKind::Shutdown);
        let reply: HandshakeReply = serde_json::from_str(
            r#"{"mcpls":9,"version":"9.0.0","pid":1,"refusal":{"reason":"something_new","detail":1}}"#,
        )
        .unwrap();
        assert_eq!(reply.refusal, Some(Refusal::Other));
        assert_eq!(reply.sessions, 0);
    }

    #[test]
    fn test_builds_order_by_protocol_then_version() {
        assert_eq!(
            compare_builds((1, "0.3.10"), (1, "0.3.9")),
            Ordering::Greater
        );
        assert_eq!(compare_builds((1, "0.3.9"), (1, "0.3.9")), Ordering::Equal);
        assert_eq!(compare_builds((1, "9.0.0"), (2, "0.1.0")), Ordering::Less);
    }

    #[test]
    fn test_only_trusted_against_ignored_conflicts() {
        let stamp = |source, project_ignored| ConfigStamp {
            fingerprint: String::new(),
            source,
            project_ignored,
        };
        let loaded = stamp(ConfigSource::Project, false);
        let ignored = stamp(ConfigSource::Global, true);
        let explicit = stamp(ConfigSource::Explicit, false);
        assert!(loaded.conflicts_with(&ignored));
        assert!(ignored.conflicts_with(&loaded));
        assert!(!loaded.conflicts_with(&explicit));
        assert!(!ignored.conflicts_with(&explicit));
        assert!(!loaded.conflicts_with(&loaded));
    }

    #[tokio::test]
    async fn test_read_consumes_exactly_one_line() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        write(&mut client, &Handshake::hook()).await.unwrap();
        client.write_all(b"after\n").await.unwrap();

        let handshake: Handshake = read(&mut server).await.unwrap();
        assert_eq!(handshake, Handshake::hook());
        let mut rest = [0u8; 6];
        server.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"after\n");
    }
}
