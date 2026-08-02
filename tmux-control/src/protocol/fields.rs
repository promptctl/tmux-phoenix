//! Low-level byte-slice field parsing shared by every message parser in
//! `message.rs` (`[LAW:one-source-of-truth]`): splitting on the wire's space
//! delimiter, the `-` "not applicable" sentinel (SPEC §7.9), and lossy
//! bytes-to-text conversion for fields the protocol treats as display text
//! (names, messages, errors) rather than byte-faithful data.

/// Split on every ASCII space, mirroring the reference parser's
/// `args.split(" ")` — trailing tokens beyond what a caller indexes are
/// tolerated, not an error (matches SPEC examples exactly).
pub fn split_ws(args: &[u8]) -> Vec<&[u8]> {
    args.split(|&b| b == b' ').collect()
}

/// Byte offset of the first ASCII space, if any.
pub fn find_space(args: &[u8]) -> Option<usize> {
    args.iter().position(|&b| b == b' ')
}

/// The first whitespace-delimited token (or the whole slice, if it has no
/// space). Cheaper than `split_ws(args)[0]` for the several message types
/// that only ever need one field — avoids allocating a `Vec` of every
/// remaining token just to read and discard it.
pub fn first_token(args: &[u8]) -> &[u8] {
    match find_space(args) {
        Some(idx) => &args[..idx],
        None => args,
    }
}

/// Byte offset of the first `" : "` separator (SPEC §7.1, §7.9: the literal
/// space-colon-space delimiter before a payload/value field).
pub fn find_colon_sep(args: &[u8]) -> Option<usize> {
    args.windows(3).position(|w| w == b" : ")
}

/// Lossy bytes → text, for fields the protocol treats as display text
/// (names, messages, errors) rather than byte-faithful payload data.
///
/// Deliberate divergence from the reference (`promptctl/tmux-control-mode-js`):
/// its JS strings are a byte-faithful Latin-1 container end-to-end
/// (`byte-codec.ts`), so a raw non-UTF-8 byte in e.g. a session name
/// round-trips exactly. Rust's `String` cannot hold arbitrary bytes, and
/// DESIGN.md §3.1 types these fields as `String`, not `Vec<u8>` — so *some*
/// conversion policy is required, and lossy (never panicking, never
/// rejecting the whole message) is the only one consistent with this
/// codec's total/panic-free guarantee. In practice tmux names/messages are
/// user- or shell-chosen text and essentially always valid UTF-8; a real
/// non-UTF-8 byte here would substitute U+FFFD rather than crash or drop
/// the message.
pub fn to_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Parse a required numeric field. Strict: the *entire* slice must be a
/// valid integer, unlike the reference's `parseInt`-based fields, which
/// parse a leading numeric prefix and ignore trailing garbage (so e.g.
/// `"12abc"` silently becomes `12` in the JS parser). That leniency isn't a
/// SPEC requirement — real tmux only ever emits pure decimal fields — and
/// silently accepting a corrupted numeric field as if it were valid runs
/// against this project's no-silent-failure stance (DESIGN.md §1): a
/// corrupted `%begin`/`%end` guard or subscription field should surface as
/// `Unknown`/`ProtocolError`, not parse into a plausible-looking wrong value.
pub fn parse_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// See [`parse_u32`].
pub fn parse_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// See [`parse_u32`].
pub fn parse_i64(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Parse a `-`-or-value field (SPEC §7.9): `-` means "not applicable" and
/// maps to `None`; anything else must parse via `f` or the whole field (and
/// therefore the message) is malformed. The outer `Option` is that
/// malformed-vs-not signal; the inner `Option` is the field's own value.
pub fn parse_optional<T>(raw: &[u8], f: impl FnOnce(&[u8]) -> Option<T>) -> Option<Option<T>> {
    if raw == b"-" {
        Some(None)
    } else {
        f(raw).map(Some)
    }
}

/// `parse_optional` specialized to a bare (unprefixed) integer field, e.g.
/// `%subscription-changed`'s `window-index`.
pub fn parse_optional_u32(raw: &[u8]) -> Option<Option<u32>> {
    parse_optional(raw, parse_u32)
}
