//! Derivation of a Windows pipe name from a configured endpoint.
//!
//! The previous rule reduced the endpoint to its **file stem**
//! (`…/takka-da-123-456/test.sock` → `\\.\pipe\takka-test`). Every
//! concurrently running daemon smoke in one test binary therefore chose
//! the same pipe name, and `FILE_FLAG_FIRST_PIPE_INSTANCE` turned the
//! losers into `ERROR_ACCESS_DENIED` (os error 5) at bind time — which
//! the client then swallowed into "no daemon running". The name is now
//! derived from a hash of the *whole normalized* endpoint, so distinct
//! endpoints cannot collide and the same endpoint always yields the same
//! name on both sides.
//!
//! The derivation is duplicated in `takka-cli`'s `pipe_id` module (the
//! runtime deliberately has no dependency on the CLI): the two copies
//! must stay identical — `pipe_name_matches_server_derivation` on the
//! CLI side pins the same vectors as
//! `pipe_name_hashing_is_unique_and_stable` here.
//!
//! `DefaultHasher` is explicitly NOT used: it is not stable across
//! processes (`RandomState` is randomly seeded), so server and client
//! would disagree. FNV-1a over the normalized UTF-8 bytes is stable,
//! dependency-free and deterministic.

/// Root every local named pipe lives under.
pub(crate) const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Positional 64-bit FNV-1a (offset basis 0xcbf29ce484222325).
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Normalize an endpoint into the string the hash is taken over.
///
/// Textual only: `std::fs::canonicalize` would fail for a socket
/// path that does not exist yet (the broker mints its endpoint
/// *before* binding) and resolves 8.3 short names differently
/// depending on the working directory. Both sides see the same
/// string (the daemon records `config.socket_path` verbatim in the
/// discovery file), so stripping trailing separators, folding
/// `/` to `\` and lowercasing (NTFS is case-insensitive; pipe names
/// are compared case-insensitively by the object manager) is enough
/// to make the mapping stable.
pub(crate) fn normalize(endpoint: &str) -> String {
    let mut text = endpoint.replace('/', r"\");
    while text.len() > 1 && text.ends_with('\\') {
        text.pop();
    }
    // ASCII-only lowercasing: a non-ASCII path component must hash
    // identically on both sides, and `to_lowercase`'s Unicode rules
    // (e.g. Turkish dotless i) would make that posture-dependent.
    text.make_ascii_lowercase();
    text
}

/// The pipe name (without the `\\.\pipe\` root) for an endpoint:
/// `takka-<hex16>`, where `hex16` is the FNV-1a of the normalized
/// endpoint. A caller that already passes a fully qualified
/// `\\.\pipe\…` path keeps it verbatim (tests pin names that way).
pub(crate) fn pipe_name_for(endpoint: &str) -> String {
    if endpoint.starts_with(PIPE_PREFIX) {
        return endpoint.to_string();
    }
    format!(
        "{PIPE_PREFIX}takka-{:016x}",
        fnv1a64(normalize(endpoint).as_bytes())
    )
}
