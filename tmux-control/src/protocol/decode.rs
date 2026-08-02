//! Octal-escape decoding for `%output`/`%extended-output` payloads (SPEC §10).
//!
//! Ported from the reference decoder (`promptctl/tmux-control-mode-js`
//! `src/protocol/decode.ts`), which is the authoritative source for this
//! library-side tolerance policy — SPEC §10 only documents the wire encoding
//! rule (`\NNN`, `\` → `\134`), not decoder recovery behavior. The reference
//! decoder's actual recovery differs slightly from DESIGN.md's prose gloss
//! ("pass a stray `\` through"): a malformed escape decodes to `?`, not a
//! literal backslash. This port follows the reference source, per this
//! project's standing rule to spec against authoritative sources/impls
//! rather than paraphrases.

const SPACE: u8 = 0x20;
const BACKSLASH: u8 = b'\\';
const CR: u8 = b'\r';
const QUESTION: u8 = b'?';

/// Decode tmux's octal-escaped output into raw bytes:
///
/// - a literal (unescaped) byte `< 0x20` is dropped — tmux always escapes
///   real control bytes as octal, so a literal one arriving unescaped is
///   transport/line-driver noise,
/// - `\NNN` (three octal digits, tolerating a stray `\r` skipped between
///   digits) decodes to one byte,
/// - a malformed escape (`\` not followed by three octal digits) decodes to
///   `?`, and parsing resumes at the byte that failed to be a digit,
/// - every other byte (`0x20..=0xFF`) passes through unchanged.
///
/// Total and panic-free: every input byte sequence produces some output.
pub fn decode_octal(input: &[u8]) -> Vec<u8> {
    let len = input.len();
    let mut result = Vec::with_capacity(len);
    let mut i = 0usize;

    while i < len {
        let mut c = input[i];

        if c < SPACE {
            i += 1;
            continue;
        }

        if c == BACKSLASH {
            let mut value: u16 = 0;
            let mut malformed = false;
            for _ in 0..3 {
                i += 1;
                while i < len && input[i] == CR {
                    i += 1;
                }
                let digit = if i < len {
                    input[i].wrapping_sub(b'0')
                } else {
                    u8::MAX
                };
                if digit > 7 {
                    i -= 1;
                    malformed = true;
                    break;
                }
                value = value * 8 + digit as u16;
            }
            c = if malformed { QUESTION } else { value as u8 };
        }

        result.push(c);
        i += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_printable_bytes() {
        assert_eq!(decode_octal(b"hello"), b"hello");
    }

    #[test]
    fn decodes_three_digit_octal_escape() {
        assert_eq!(decode_octal(b"\\033"), vec![27]);
        assert_eq!(decode_octal(b"\\000"), vec![0]);
        assert_eq!(decode_octal(b"\\377"), vec![255]);
    }

    #[test]
    fn decodes_escaped_backslash() {
        assert_eq!(decode_octal(b"\\134"), vec![0x5c]);
    }

    #[test]
    fn drops_literal_control_bytes() {
        assert_eq!(decode_octal(b"a\x01\x02b"), b"ab");
    }

    #[test]
    fn skips_stray_cr_between_escape_digits() {
        // A line-driver may interleave \r within the three digit positions;
        // the decoder skips it and still decodes the intended byte.
        assert_eq!(decode_octal(b"\\0\r33"), vec![27]);
    }

    #[test]
    fn malformed_escape_becomes_question_mark_and_resumes_at_the_bad_byte() {
        assert_eq!(decode_octal(b"\\9x"), b"?9x");
    }

    #[test]
    fn truncated_trailing_escape_becomes_question_mark() {
        assert_eq!(decode_octal(b"ab\\"), b"ab?");
        // The lone leading digit '1' is consumed into the failed escape
        // attempt (mirrors the reference decoder exactly) rather than
        // being reprocessed as a literal byte.
        assert_eq!(decode_octal(b"ab\\1"), b"ab?");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(decode_octal(b""), Vec::<u8>::new());
    }
}
