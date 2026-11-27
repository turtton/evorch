//! What the GUI records about losing the core and getting it back, in the log
//! file `gui.log` (see [`crate::logging`]). The window shows only a status line;
//! these lines keep the reason of the close (the `TransportError`), the stage the
//! recovery reached, how long the loss has lasted and how many automatic
//! attempts were used, so a disconnect can be explained afterwards.
use std::time::{Duration, Instant};
use workbench_client::TransportError;

/// One loss of the connection and the recovery that follows, until a session is
/// open again.
#[derive(Default)]
pub struct Episode {
    lost_at: Option<Instant>,
    losses: u32,
}

/// How a session ended.
pub struct SessionEnd<'a> {
    /// Why the client closed the connection, when it did.
    pub transport: Option<&'a TransportError>,
    pub session: Duration,
    pub orderly: bool,
    pub planned_restart: bool,
    pub reconnect_requested: bool,
    pub resync: bool,
    /// The loss counts as unexpected: automatic recovery follows.
    pub abnormal: bool,
}

impl Episode {
    /// The connection was lost (`abnormal`) or the session ended some other way.
    pub fn session_ended(&mut self, now: Instant, end: &SessionEnd<'_>) {
        if end.abnormal {
            self.lost_at.get_or_insert(now);
            self.losses += 1;
        }
        let transport = end.transport.map(ToString::to_string);
        let transport_debug = end.transport.map(|error| format!("{error:?}"));
        let session_ms = millis(end.session);
        let losses = self.losses;
        if end.abnormal {
            tracing::warn!(
                stage = "session",
                transport = ?transport,
                transport_debug = ?transport_debug,
                session_ms,
                losses,
                orderly = end.orderly,
                planned_restart = end.planned_restart,
                "lost the connection to the core"
            );
        } else {
            tracing::info!(
                stage = "session",
                transport = ?transport,
                session_ms,
                orderly = end.orderly,
                planned_restart = end.planned_restart,
                reconnect_requested = end.reconnect_requested,
                resync = end.resync,
                "the session with the core ended"
            );
        }
    }

    /// Milliseconds since the first unexpected loss of this episode.
    pub fn since_loss_ms(&self, now: Instant) -> Option<u64> {
        self.lost_at
            .map(|lost| millis(now.saturating_duration_since(lost)))
    }

    /// A session is open again (or for the first time).
    pub fn recovered(&mut self, now: Instant) {
        tracing::info!(
            stage = "session-open",
            since_loss_ms = ?self.since_loss_ms(now),
            losses = self.losses,
            "the session with the core is open"
        );
        self.lost_at = None;
        self.losses = 0;
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured(run: impl FnOnce()) -> String {
        let log = Captured::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(log.0.lock().unwrap().clone()).unwrap()
    }

    fn lost<'a>(transport: &'a TransportError, session: Duration) -> SessionEnd<'a> {
        SessionEnd {
            transport: Some(transport),
            session,
            orderly: false,
            planned_restart: false,
            reconnect_requested: false,
            resync: false,
            abnormal: true,
        }
    }

    #[test]
    fn a_lost_connection_is_recorded_with_its_transport_error_and_how_often_it_was_lost() {
        let mut episode = Episode::default();
        let first = Instant::now();
        let reason = TransportError::ProtocolViolation("bad frame".into());
        let text = captured(|| {
            episode.session_ended(first, &lost(&reason, Duration::from_millis(2500)));
            episode.session_ended(
                first + Duration::from_secs(4),
                &lost(&TransportError::Disconnected, Duration::from_secs(1)),
            );
        });
        let mut lines = text.lines();
        let line = lines.next().unwrap();
        for expected in [
            "WARN",
            "lost the connection to the core",
            "protocol violation: bad frame",
            "ProtocolViolation",
            "session_ms=2500",
            "losses=1",
        ] {
            assert!(line.contains(expected), "{expected} missing in {line}");
        }
        let line = lines.next().unwrap();
        assert!(
            line.contains("disconnected from the core") && line.contains("losses=2"),
            "{line}"
        );
        assert_eq!(
            episode.since_loss_ms(first + Duration::from_millis(1500)),
            Some(1500),
            "measured from the first loss of the episode"
        );
        let after = captured(|| episode.recovered(first + Duration::from_secs(9)));
        assert!(
            after.contains("since_loss_ms=Some(9000)") && after.contains("losses=2"),
            "{after}"
        );
        assert_eq!(
            episode.since_loss_ms(first),
            None,
            "a new session starts over"
        );
    }

    #[test]
    fn an_orderly_stop_is_not_reported_as_a_lost_connection() {
        let mut episode = Episode::default();
        let text = captured(|| {
            episode.session_ended(
                Instant::now(),
                &SessionEnd {
                    orderly: true,
                    abnormal: false,
                    ..lost(&TransportError::Disconnected, Duration::from_secs(5))
                },
            );
        });
        assert!(
            text.contains("INFO") && text.contains("orderly=true"),
            "{text}"
        );
        assert!(!text.contains("lost the connection"), "{text}");
        assert_eq!(episode.since_loss_ms(Instant::now()), None);
    }
}
