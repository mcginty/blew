use std::time::Duration;

/// L2CAP Protocol Service Multiplexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Psm(pub u16);

impl Psm {
    #[must_use]
    pub fn value(self) -> u16 {
        self.0
    }
}

impl From<u16> for Psm {
    fn from(v: u16) -> Self {
        Self(v)
    }
}

impl std::fmt::Display for Psm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Default for [`L2capConfig::buffer_size`].
pub const DEFAULT_L2CAP_BUFFER_SIZE: usize = 64 * 1024;
/// Default for [`L2capConfig::read_chunk_size`].
pub const DEFAULT_L2CAP_READ_CHUNK_SIZE: usize = 4096;
/// Default for [`L2capConfig::linger_timeout`].
pub const DEFAULT_L2CAP_LINGER_TIMEOUT: Duration = Duration::from_secs(1);

/// Floor applied to [`L2capConfig::buffer_size`].
///
/// A zero-capacity buffer is not a tight bound, it is a deadlock:
/// `tokio::io::duplex(0)` never accepts a write, so neither direction could
/// make progress.
pub const MIN_L2CAP_BUFFER_SIZE: usize = 1024;
/// Floor applied to [`L2capConfig::read_chunk_size`]. A zero-length read
/// makes no progress either.
pub const MIN_L2CAP_READ_CHUNK_SIZE: usize = 64;

/// Link security demanded of an L2CAP CoC channel.
///
/// # What this actually guarantees
///
/// Where a backend accepts a level, the OS or controller **enforces** it. This
/// is a requirement, not a hint, and there is no advisory middle ground: a level
/// is either enforced by the platform or unavailable there.
///
/// - **Linux**, both roles: `BT_SECURITY_*` on the socket. A listener rejects
///   inbound connections that don't meet the level; an opener elevates the link
///   via SMP before the connect completes, and fails the connect if it can't.
/// - **Android**, both roles: `listenUsingL2capChannel` /
///   `createL2capChannel`. The stack pairs the peer or refuses the socket.
/// - **Apple peripheral**: `publishL2CAPChannelWithEncryption:YES`. The
///   controller refuses unencrypted connections to that PSM.
///
/// Three combinations have no mechanism at all. They are refused with
/// [`BlewError::L2capEncryptionUnsupported`](crate::error::BlewError::L2capEncryptionUnsupported)
/// rather than silently ignored, because a setting that quietly does nothing on
/// one platform is worse than one that fails loudly:
///
/// - [`RequireAuthentication`](Self::RequireAuthentication) on an **Apple
///   peripheral** — `publishL2CAPChannelWithEncryption:` is a single boolean and
///   says nothing about whether the pairing was MITM-protected.
/// - Either level on an **Apple central** — see the note under the table.
///
/// If you need encryption between two blew peers and one of them is an Apple
/// central, set it on the peripheral: the requirement is enforced there, and
/// CoreBluetooth pairs automatically when the CoC connect is refused, so the
/// channel comes up encrypted anyway.
///
/// # How each end enforces it
///
/// LE encryption is a property of the ACL link, not of one channel, and the two
/// ends enforce a requirement by different mechanisms. The listener enforces by
/// *refusing*: `LE_CREDIT_BASED_CONNECTION_REQ` carries no security field, so a
/// PSM whose requirement is unmet answers with "insufficient
/// authentication/encryption" and the channel never opens. The opener enforces
/// by *elevating*: it raises the link's security before the request goes out,
/// which on LE only the Central can actually actuate (a Peripheral can only send
/// an SMP Security Request and ask). So this field is meaningful on both
/// [`Central::open_l2cap_channel`](crate::Central::open_l2cap_channel) and
/// [`Peripheral::l2cap_listener`](crate::Peripheral::l2cap_listener) — it just
/// reaches the same guarantee from opposite directions.
///
/// # Platform mapping
///
/// Platforms express this at very different resolutions, so a backend maps the
/// requested level onto the nearest level its API can express that is **never
/// weaker** than what was asked for — rounding *up* where a platform is coarse,
/// and refusing outright where it has nothing.
///
/// | | `Insecure` | `RequireEncryption` | `RequireAuthentication` |
/// |---|---|---|---|
/// | Apple peripheral | `publishL2CAPChannelWithEncryption:NO` | `…WithEncryption:YES` | unsupported |
/// | Apple central | `openL2CAPChannel:` | unsupported | unsupported |
/// | Android | `…InsecureL2capChannel` | `…L2capChannel` (stronger) | `…L2capChannel` |
/// | Linux | `BT_SECURITY_LOW` | `BT_SECURITY_MEDIUM` | `BT_SECURITY_HIGH` |
///
/// The Apple central row is an API gap, not a protocol one. Linux and Android
/// both elevate the link from the opening side — `BT_SECURITY_*` on a connecting
/// socket, and `createL2capChannel`'s authenticated-and-encrypted contract — but
/// CoreBluetooth exposes no way to pair or raise security on demand, so an Apple
/// central can only take whatever the peer's PSM happens to insist on — it can
/// neither demand nor verify anything itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum L2capEncryption {
    /// No link security requested: the channel may carry plaintext over an
    /// unbonded link. This is the default, and what every backend did before
    /// the setting existed.
    #[default]
    Insecure,
    /// The link must be encrypted. Unauthenticated ("Just Works") pairing
    /// satisfies this.
    RequireEncryption,
    /// The link must be encrypted *and* the pairing authenticated
    /// (MITM-protected).
    RequireAuthentication,
}

impl std::fmt::Display for L2capEncryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Insecure => "insecure",
            Self::RequireEncryption => "require-encryption",
            Self::RequireAuthentication => "require-authentication",
        })
    }
}

/// Tuning for a single L2CAP channel.
///
/// Construct with `..Default::default()` so a new field costs you one
/// recompile rather than an edit at every call site.
///
/// # Platform caveats
///
/// **Linux observes none of the buffering fields.** `bluer::l2cap::Stream` is
/// already an async byte stream, so the backend hands it to the caller directly
/// and the kernel socket buffers provide flow control. There is no in-process
/// bridge to size and nothing queued locally to flush. Apple and Android both
/// marshal bytes between a platform socket and the async world, so they observe
/// all three. [`encryption`](Self::encryption) is honoured everywhere; see
/// [`L2capEncryption`] for how each platform coarsens it.
#[derive(Debug, Clone)]
pub struct L2capConfig {
    /// Bytes buffered in each direction between the application and the
    /// platform socket.
    ///
    /// Once buffering fills, the backend stops reading from the platform
    /// socket, which stops L2CAP credits being returned to the peer, which
    /// stops the peer transmitting — the protocol's own flow control does the
    /// work. Larger values trade memory per channel for tolerance of bursty
    /// readers.
    ///
    /// Each direction holds this much twice: once in the stream buffer the
    /// application reads and writes, and once in the queue handing bytes to the
    /// platform socket. Budget roughly `4 * buffer_size` per open channel.
    ///
    /// Raised to [`MIN_L2CAP_BUFFER_SIZE`], and to `read_chunk_size`, if set
    /// lower — a buffer too small to hold one read would stall rather than
    /// throttle.
    pub buffer_size: usize,
    /// Largest read issued against the platform socket at once.
    ///
    /// Raised to [`MIN_L2CAP_READ_CHUNK_SIZE`] if set lower.
    pub read_chunk_size: usize,
    /// How long the backend keeps a closing channel alive to finish writing
    /// whatever is still queued, before tearing it down regardless.
    ///
    /// Modelled on `SO_LINGER`. Closing is asynchronous: both
    /// [`L2capChannel::close`](crate::L2capChannel::close) and dropping the
    /// channel hand it to the backend, which keeps draining until the queue
    /// empties or this deadline passes. Neither blocks the caller, so `Drop`
    /// gets the same delivery guarantee `close()` does — which matters, because
    /// dropping is by far the more common way an `AsyncWrite` goes away.
    ///
    /// `None` drains indefinitely and never forces teardown; use it only when
    /// the peer is trusted to keep accepting data.
    ///
    /// Note this is delivery to the *platform socket*, not acknowledgement by
    /// the peer. Nothing here waits for the far end to read.
    pub linger_timeout: Option<Duration>,
    /// Link security demanded when publishing or opening a channel.
    ///
    /// Defaults to [`L2capEncryption::Insecure`], which is what every backend
    /// did before this field existed. Raising it is a *requirement*, not a
    /// preference: the platform enforces it, which can mean triggering pairing
    /// on the first channel, and a backend with no way to express the level you
    /// asked for fails the call rather than substituting a weaker one. See
    /// [`L2capEncryption`] for what each platform enforces and where the three
    /// gaps are.
    pub encryption: L2capEncryption,
}

// Only the bridged backends size a buffer; Linux hands `bluer::l2cap::Stream`
// straight to the caller and has nothing to apply these to.
#[cfg_attr(
    not(any(target_vendor = "apple", target_os = "android")),
    allow(dead_code)
)]
impl L2capConfig {
    /// `read_chunk_size` with the floor applied.
    #[must_use]
    pub(crate) fn effective_read_chunk_size(&self) -> usize {
        self.read_chunk_size.max(MIN_L2CAP_READ_CHUNK_SIZE)
    }

    /// `buffer_size` with the floors applied. Never smaller than one read,
    /// so a full chunk always has somewhere to land.
    #[must_use]
    pub(crate) fn effective_buffer_size(&self) -> usize {
        self.buffer_size
            .max(MIN_L2CAP_BUFFER_SIZE)
            .max(self.effective_read_chunk_size())
    }
}

impl Default for L2capConfig {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_L2CAP_BUFFER_SIZE,
            read_chunk_size: DEFAULT_L2CAP_READ_CHUNK_SIZE,
            linger_timeout: Some(DEFAULT_L2CAP_LINGER_TIMEOUT),
            encryption: L2capEncryption::Insecure,
        }
    }
}

/// Why an L2CAP channel stopped carrying data.
///
/// Retrieved with [`L2capChannel::close_reason`](crate::L2capChannel::close_reason).
/// Anything other than [`Closed`](Self::Closed) also surfaces from `AsyncRead`
/// as an `io::Error` rather than a clean end-of-stream, so a dropped link is not
/// mistaken for the peer politely hanging up.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum L2capCloseReason {
    /// Either end closed the channel deliberately. Reads report EOF.
    Closed,
    /// The underlying ACL connection went away.
    LinkLost,
    /// The platform transport reported an error.
    TransportError(String),
}

impl L2capCloseReason {
    /// Convert to the `io::Error` that `AsyncRead` should report, or `None` for
    /// a clean close (which stays an end-of-stream).
    pub(crate) fn as_io_error(&self) -> Option<std::io::Error> {
        match self {
            Self::Closed => None,
            Self::LinkLost => Some(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "L2CAP link lost",
            )),
            Self::TransportError(msg) => Some(std::io::Error::other(format!(
                "L2CAP transport error: {msg}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_returned_unchanged() {
        let config = L2capConfig::default();
        assert_eq!(config.effective_buffer_size(), DEFAULT_L2CAP_BUFFER_SIZE);
        assert_eq!(
            config.effective_read_chunk_size(),
            DEFAULT_L2CAP_READ_CHUNK_SIZE
        );
    }

    #[test]
    fn encryption_defaults_to_insecure() {
        // Raising this default would silently start triggering pairing for
        // every existing caller, so it stays put until a major bump.
        assert_eq!(L2capConfig::default().encryption, L2capEncryption::Insecure);
    }

    #[test]
    fn zero_sizes_are_raised_to_the_floor() {
        // A zero here would deadlock rather than throttle: duplex(0) never
        // accepts a write and a zero-length read never progresses.
        let config = L2capConfig {
            buffer_size: 0,
            read_chunk_size: 0,
            ..Default::default()
        };
        assert_eq!(config.effective_buffer_size(), MIN_L2CAP_BUFFER_SIZE);
        assert_eq!(
            config.effective_read_chunk_size(),
            MIN_L2CAP_READ_CHUNK_SIZE
        );
    }

    #[test]
    fn buffer_is_never_smaller_than_one_read() {
        let config = L2capConfig {
            buffer_size: 2048,
            read_chunk_size: 16 * 1024,
            ..Default::default()
        };
        assert_eq!(config.effective_buffer_size(), 16 * 1024);
    }
}
