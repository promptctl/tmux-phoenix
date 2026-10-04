//! `phoenix-store` — atomic, versioned, generational persistence for a
//! `phoenix-core` `Snapshot` (ARCHITECTURE.md §7).

mod binary;
mod blob_store;
mod checksum;
mod codec;
mod codec_v1;
mod error;
mod header;
mod json;
mod store;
mod tag;
mod version;

pub use error::StoreError;
pub use json::to_json;
pub use store::{GenerationInfo, Retention, SaveOutcome, Store, Unreadable};
pub use tag::Tag;
pub use version::FormatVersion;

/// The one sample graph and scratch directory every module's tests share
/// (`[LAW:one-source-of-truth]` for fixtures).
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use phoenix_core::{
        Client, ClientName, Content, ContentFailure, Cwd, Foreground, GenerationId, GroupName,
        HistoryIndicator, Layout, Made, NonEmpty, OffsetDateTime, Origin, Pane, PaneId, PaneIndex,
        RecoveryFailure, ServerId, Session, SessionName, Snapshot, TmuxVersion, Touched, WinLink,
        Window, WindowId, WindowIndex, WindowName,
    };

    use crate::blob_store::BlobStore;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "phoenix-store-test-{name}-{}-{nanos}-{n}",
                std::process::id()
            )))
        }

        pub fn blobs(&self) -> BlobStore {
            BlobStore::new(self.0.join("blobs"))
        }

        pub fn blob_count(&self) -> usize {
            std::fs::read_dir(self.0.join("blobs"))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
                .count()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn captured(lines: &[&str]) -> Content {
        Content::Captured {
            indicator: HistoryIndicator {
                history_size: lines.len() as u64,
                history_bytes: 100,
            },
            scrollback: lines.iter().map(|l| l.to_string()).collect(),
            visible: lines.last().map(|l| l.to_string()).into_iter().collect(),
        }
    }

    /// Two sessions in a group sharing a two-pane window, plus a window
    /// only the first links; every variant of every absence appears once.
    pub fn sample_snapshot(captured_at: i64) -> Snapshot {
        let shell = Pane {
            id: PaneId(7),
            index: PaneIndex(0),
            cwd: Cwd::Unreadable,
            foreground: Foreground::Shell,
            content: Content::NotCaptured {
                reason: ContentFailure::CapturePane {
                    message: "pane is dead".to_string(),
                },
            },
        };
        let vim = Pane {
            id: PaneId(9),
            index: PaneIndex(1),
            cwd: Cwd::parse("/home/user/proj"),
            foreground: Foreground::Program {
                argv: NonEmpty::new("vim".to_string(), vec!["DESIGN.md".to_string()]),
            },
            content: captured(&["line one", "line two"]),
        };
        let gone = Pane {
            id: PaneId(11),
            index: PaneIndex(0),
            cwd: Cwd::parse("/"),
            foreground: Foreground::Unrecovered {
                reason: RecoveryFailure::Os {
                    message: "permission denied".to_string(),
                },
            },
            content: captured(&["only"]),
        };
        let shared = Window::new(
            WindowId(0),
            Made::ByPhoenix {
                generation: GenerationId(5),
                saved: WindowId(3),
            },
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            true,
            NonEmpty::new(shell, vec![vim]),
            PaneIndex(1),
        )
        .unwrap();
        let own = Window::new(
            WindowId(1),
            Made::NotByPhoenix,
            WindowName::parse("logs").unwrap(),
            Layout::parse("b25e,80x24,0,0,1").unwrap(),
            false,
            NonEmpty::singleton(gone),
            PaneIndex(0),
        )
        .unwrap();
        let link = |index: u32, window: u32| WinLink {
            index: WindowIndex(index),
            window: WindowId(window),
        };
        let main = Session::new(
            SessionName::parse("main").unwrap(),
            GroupName::parse("main"),
            NonEmpty::new(link(1, 0), vec![link(2, 1)]),
            WindowIndex(2),
            Some(WindowIndex(1)),
        )
        .unwrap();
        let twin = Session::new(
            SessionName::parse("twin").unwrap(),
            GroupName::parse("main"),
            NonEmpty::singleton(link(1, 0)),
            WindowIndex(1),
            None,
        )
        .unwrap();
        Snapshot::new(
            Origin::Recorded(ServerId {
                pid: 4242,
                start_time: 1_700_000_000,
            }),
            Touched::By(GenerationId(5)),
            OffsetDateTime::from_unix_timestamp(captured_at),
            TmuxVersion { major: 3, minor: 7 },
            NonEmpty::new(shared, vec![own]),
            NonEmpty::new(main, vec![twin]),
            vec![Client {
                name: ClientName::parse("/dev/ttys003").unwrap(),
                session: SessionName::parse("main").unwrap(),
            }],
        )
        .unwrap()
    }
}
