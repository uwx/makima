//! Sharing a session that is already running: the methods a client asks for a
//! listener with, and the codes it can be refused with. The link, the token
//! and the announcement live in `makima-lan`.

/// Client to server: listen for other machines, answering the link and the
/// token they need.
pub const SHARE_METHOD: &str = "makima/share";
/// Client to server: stop the listener this connection opened.
pub const UNSHARE_METHOD: &str = "makima/unshare";
/// Answers a share request that could not bind or read the token.
pub const SHARE_FAILED_CODE: i32 = -32013;
/// Answers a share request from a connection that only watches the session.
pub const NOT_OWNER_CODE: i32 = -32014;
pub const NOT_OWNER_MESSAGE: &str = "only the connection that owns the session can share it";
