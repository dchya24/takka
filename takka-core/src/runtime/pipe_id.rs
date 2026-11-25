//! Derivation of a Windows named-pipe name from a configured endpoint.
//!
//! Shared by `takka-runtime` (server bind) and `takka-cli` (client attach)
//! so both sides derive the exact same pipe name without duplicating the
//! hashing rule.
//!
//! `DefaultHasher` is explicitly NOT used: it is not stable across
//! processes (`RandomState` is randomly seeded), so server and client
//! would disagree. FNV-1a over the normalized UTF-8 bytes is stable,
//! dependency-free and deterministic.

/// Root every local named pipe lives under.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Positional 64-bit FNV-1a (offset basis 0xcbf29ce484222325).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
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
/// `/` to `\` and ASCII-lowercasing (NTFS is case-insensitive; pipe
/// names are compared case-insensitively by the object manager) is
/// enough to make the mapping stable.
pub fn normalize(endpoint: &str) -> String {
    let mut text = endpoint.replace('/', r"\");
    while text.len() > 1 && text.ends_with('\\') {
        text.pop();
    }
    text.make_ascii_lowercase();
    text
}

/// The pipe name (with the `\\.\pipe\` root) for an endpoint:
/// `\\.\pipe\takka-<hex16>`, where `hex16` is the FNV-1a of the
/// normalized endpoint. A caller that already passes a fully qualified
/// `\\.\pipe\…` path keeps it verbatim (tests pin names that way).
pub fn pipe_name_for(endpoint: &str) -> String {
    if endpoint.starts_with(PIPE_PREFIX) {
        return endpoint.to_string();
    }
    format!(
        "{PIPE_PREFIX}takka-{:016x}",
        fnv1a64(normalize(endpoint).as_bytes())
    )
}
