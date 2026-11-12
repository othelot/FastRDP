//! Public keys that may sign FastRDP releases (raw Ed25519, base64). The first is the current key,
//! the second the next one: after a key rotation, installations that only know the old pair still
//! accept releases signed with the next key. The private keys are never in this repository.
pub const TRUSTED_KEYS: &[&str] = &[
    // 1: current (key id 118f186f6fb6a31e)
    "bBgTVbIPZHT4gZIKCAm0ZwLoVtk1XWHLKNXjEMJLYLM=",
    // 2: next, for rotation (key id 61fa02da77880474)
    "PFufVEKlgQ+qQSlMniE75+j6jaNdaXewz6oKOByU5BQ=",
];
