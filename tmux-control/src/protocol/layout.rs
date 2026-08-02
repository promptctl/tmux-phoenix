//! Opaque wrapper around a tmux layout descriptor string.
//!
//! tmux owns window geometry; we transport its layout strings verbatim and
//! never re-derive or re-parse them (`[LAW:one-source-of-truth]`).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout(pub String);
