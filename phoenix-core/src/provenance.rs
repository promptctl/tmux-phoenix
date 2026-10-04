//! Where a snapshot came from and what phoenix has already touched
//! (ARCHITECTURE.md §3): facts recorded where they are born — the server
//! option `@phoenix-generation`, the window option `@phoenix-window`, the
//! server's `#{pid}:#{start_time}` — and read as values downstream. Each
//! absence is a variant with a meaning, never a bare `Option`
//! (`[LAW:types-are-the-program]`).

use std::fmt;

use crate::ids::{GenerationId, ServerId, WindowId};

/// Which server incarnation a snapshot was captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    Recorded(ServerId),
    /// Only the pre-origin store format decodes to this: the generation was
    /// written before capture recorded a `ServerId`.
    BeforeOriginWasRecorded,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Recorded(id) => write!(f, "{id}"),
            Origin::BeforeOriginWasRecorded => f.write_str("-"),
        }
    }
}

/// Whether phoenix has saved from or restored onto this server incarnation:
/// the server option `@phoenix-generation`, which lives exactly as long as
/// the incarnation (verified live on tmux 3.7b: `set-option -s @…`
/// round-trips through `show-options -s -v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Touched {
    By(GenerationId),
    Never,
}

/// Whether a live window is one phoenix built: the window option
/// `@phoenix-window=<generation>:<saved window id>`, stamped by the restore
/// plan as the last step of the window's group. A window option follows the
/// window through `link-window` (verified live), so a shared window carries
/// one stamp however many sessions link it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Made {
    ByPhoenix {
        generation: GenerationId,
        saved: WindowId,
    },
    NotByPhoenix,
}

/// A `@phoenix-window` value that is set but is not `<generation>:@<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedWindowMark {
    pub value: String,
}

impl fmt::Display for MalformedWindowMark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "@phoenix-window value {:?} is not <generation>:@<window id>",
            self.value
        )
    }
}

impl std::error::Error for MalformedWindowMark {}

impl Made {
    /// The one place the option's string shape is defined
    /// (`[LAW:one-source-of-truth]`): the plan writes
    /// [`Made::option_value`] and capture reads [`Made::parse_option`].
    /// An unset option reads as the empty string in a tmux format, which is
    /// `NotByPhoenix`.
    pub fn parse_option(value: &str) -> Result<Self, MalformedWindowMark> {
        if value.is_empty() {
            return Ok(Made::NotByPhoenix);
        }
        let malformed = || MalformedWindowMark {
            value: value.to_string(),
        };
        let (generation, saved) = value.split_once(':').ok_or_else(malformed)?;
        Ok(Made::ByPhoenix {
            generation: GenerationId::parse(generation).ok_or_else(malformed)?,
            saved: WindowId::parse(saved).ok_or_else(malformed)?,
        })
    }

    /// What the plan sets `@phoenix-window` to for a window it built.
    pub fn option_value(generation: GenerationId, saved: WindowId) -> String {
        format!("{generation}:{saved}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_mark_round_trips_through_its_option_value() {
        let value = Made::option_value(GenerationId(7), WindowId(3));
        assert_eq!(value, "7:@3");
        assert_eq!(
            Made::parse_option(&value),
            Ok(Made::ByPhoenix {
                generation: GenerationId(7),
                saved: WindowId(3),
            })
        );
    }

    #[test]
    fn an_unset_window_mark_is_not_by_phoenix() {
        assert_eq!(Made::parse_option(""), Ok(Made::NotByPhoenix));
    }

    #[test]
    fn a_set_but_malformed_window_mark_is_an_error_not_a_guess() {
        for value in ["7", "7:3", "x:@3", ":@3"] {
            assert!(Made::parse_option(value).is_err(), "{value}");
        }
    }

    #[test]
    fn origin_displays_its_server_id_or_a_dash() {
        assert_eq!(
            Origin::Recorded(ServerId {
                pid: 1,
                start_time: 2
            })
            .to_string(),
            "1:2"
        );
        assert_eq!(Origin::BeforeOriginWasRecorded.to_string(), "-");
    }
}
