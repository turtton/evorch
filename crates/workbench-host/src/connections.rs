//! Which clients are connected, and a notice for the areas when one leaves
//! (ADR 0045 §10).
//!
//! A transport that serves a client over a connection of its own (the Unix
//! socket) reports it here: [`ConnectionHooks::open`] when the handshake was
//! accepted, and the guard it returns when the connection ends, whatever the
//! reason. An area that started work for a client registers a hook with
//! [`ConnectionHooks::register`] and stops what that client started when the
//! hook runs.
//!
//! * **Client, not connection.** A client may hold more than one connection at
//!   a time; the hooks run when the last one of the client ends.
//! * **Epoch.** The connections a client holds from the one that takes it from
//!   none to one until the one that ends its last make one [`ConnectionEpoch`];
//!   a later connection of the same client is a later epoch. A hook is told the
//!   epoch that ended, and an area records [`ConnectionHooks::epoch_of`] when it
//!   starts work: a notice that reaches the area late, after the client came
//!   back, then stops only what started in the epoch that ended or before it,
//!   never what the client started since.
//! * **Synchronous, short.** A hook runs in the connection's teardown, on the
//!   task that ends it. It must not wait: it only records a request that the
//!   area's own task carries out. No lock of this registry is held while a hook
//!   runs, so a hook may open or close connections, or register another hook.
//! * **No other effect.** Nothing here ends a run, or anything else, by
//!   itself: a connection that ends without an area that registered a hook
//!   changes nothing (a GUI that leaves does not stop its runs).
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use workbench_protocol::ids::ClientId;

/// One run of connections of a client, from its first to its last
/// (see the module documentation). Later epochs of a client compare greater;
/// the default is before every epoch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionEpoch(u64);

/// What an area does when a client has no connection left: it gets the client
/// and the epoch that ended.
pub type ConnectionClosedHook = Arc<dyn Fn(&ClientId, ConnectionEpoch) + Send + Sync>;

/// The open connections by client and the hooks to run when a client leaves.
#[derive(Default)]
pub struct ConnectionHooks {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    open: HashMap<ClientId, Open>,
    /// The epochs handed out so far; the next one is one more.
    epochs: u64,
    hooks: Vec<ConnectionClosedHook>,
}

struct Open {
    connections: usize,
    epoch: ConnectionEpoch,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ConnectionHooks {
    /// Run `hook` whenever a client's last connection ends.
    pub fn register(&self, hook: ConnectionClosedHook) {
        lock(&self.state).hooks.push(hook);
    }

    /// A connection of `client` is open until the returned guard is dropped.
    pub fn open(self: &Arc<Self>, client: ClientId) -> ClientConnection {
        let epoch = {
            let mut state = lock(&self.state);
            let state = &mut *state;
            let open = state.open.entry(client.clone()).or_insert_with(|| {
                state.epochs += 1;
                Open {
                    connections: 0,
                    epoch: ConnectionEpoch(state.epochs),
                }
            });
            open.connections += 1;
            open.epoch
        };
        ClientConnection {
            hooks: self.clone(),
            client,
            epoch,
        }
    }

    /// How many connections `client` holds.
    pub fn connections_of(&self, client: &ClientId) -> usize {
        lock(&self.state)
            .open
            .get(client)
            .map_or(0, |open| open.connections)
    }

    /// The epoch of `client`'s connections now, `None` while it holds none.
    pub fn epoch_of(&self, client: &ClientId) -> Option<ConnectionEpoch> {
        lock(&self.state).open.get(client).map(|open| open.epoch)
    }

    fn closed(&self, client: &ClientId) {
        let (hooks, epoch) = {
            let mut state = lock(&self.state);
            let Some(open) = state.open.get_mut(client) else {
                return;
            };
            open.connections -= 1;
            if open.connections > 0 {
                return;
            }
            let epoch = open.epoch;
            state.open.remove(client);
            (state.hooks.clone(), epoch)
        };
        // This runs in a guard's drop, possibly while another panic unwinds: a
        // hook that panics must neither escape nor keep the later hooks from
        // running, so each one is isolated and a failure is only logged. The
        // payload of the panic is a value of the hook's choosing whose own drop
        // may panic, so it is leaked rather than dropped here.
        for hook in hooks {
            if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| hook(client, epoch)))
            {
                std::mem::forget(payload);
                tracing::warn!(client = %client.as_str(), "a connection-closed hook panicked");
            }
        }
    }
}

/// One open connection of a client; dropping it ends the connection.
pub struct ClientConnection {
    hooks: Arc<ConnectionHooks>,
    client: ClientId,
    epoch: ConnectionEpoch,
}

impl ClientConnection {
    /// The epoch this connection belongs to.
    pub fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.hooks.closed(&self.client);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn client(name: &str) -> ClientId {
        ClientId::new(name).unwrap()
    }

    fn recording(hooks: &ConnectionHooks) -> Arc<Mutex<Vec<String>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        hooks.register(Arc::new(move |client, _| {
            sink.lock().unwrap().push(client.as_str().to_owned());
        }));
        seen
    }

    #[test]
    fn the_hooks_run_when_the_last_connection_of_a_client_ends() {
        // Given: a client with two connections and another client with one.
        let hooks = Arc::new(ConnectionHooks::default());
        let seen = recording(&hooks);
        let first = hooks.open(client("cli-a"));
        let second = hooks.open(client("cli-a"));
        let other = hooks.open(client("cli-b"));
        assert_eq!(hooks.connections_of(&client("cli-a")), 2);

        // When: one connection of the first client ends.
        drop(first);

        // Then: the client is still connected and nothing ran.
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(hooks.connections_of(&client("cli-a")), 1);

        // When: its last connection ends.
        drop(second);

        // Then: the hook ran once, for that client only.
        assert_eq!(*seen.lock().unwrap(), ["cli-a"]);
        assert_eq!(hooks.connections_of(&client("cli-a")), 0);
        assert_eq!(hooks.connections_of(&client("cli-b")), 1);

        // And: the other client leaving runs it for that client.
        drop(other);
        assert_eq!(*seen.lock().unwrap(), ["cli-a", "cli-b"]);
    }

    #[test]
    fn a_client_that_connects_again_is_a_new_client_for_the_hooks() {
        // Given: a client that left.
        let hooks = Arc::new(ConnectionHooks::default());
        let seen = recording(&hooks);
        drop(hooks.open(client("cli-a")));

        // When: it connects and leaves again.
        drop(hooks.open(client("cli-a")));

        // Then: the hooks ran for each departure.
        assert_eq!(*seen.lock().unwrap(), ["cli-a", "cli-a"]);
    }

    #[test]
    fn a_hook_may_use_the_registry_without_deadlock() {
        // Given: a hook that opens a connection and registers another hook.
        let hooks = Arc::new(ConnectionHooks::default());
        let inner = hooks.clone();
        let reopened = Arc::new(Mutex::new(Vec::new()));
        let sink = reopened.clone();
        hooks.register(Arc::new(move |client, _| {
            if client.as_str() == "cli-a" {
                sink.lock()
                    .unwrap()
                    .push(inner.open(ClientId::new("cli-c").unwrap()));
                inner.register(Arc::new(|_, _| {}));
            }
        }));

        // When: the client leaves.
        drop(hooks.open(client("cli-a")));

        // Then: the hook ran to its end and its connection is open.
        assert_eq!(hooks.connections_of(&client("cli-c")), 1);
    }

    #[test]
    fn a_hook_that_panics_neither_escapes_nor_stops_the_later_hooks() {
        // Given: a hook that panics, registered before one that records.
        let hooks = Arc::new(ConnectionHooks::default());
        hooks.register(Arc::new(|_, _| panic!("a hook failed")));
        let seen = recording(&hooks);
        let connection = hooks.open(client("cli-a"));

        // When: the client leaves.
        drop(connection);

        // Then: the drop returned, the later hook ran, and the client is gone.
        assert_eq!(*seen.lock().unwrap(), ["cli-a"]);
        assert_eq!(hooks.connections_of(&client("cli-a")), 0);
    }

    /// A panic payload whose own drop panics.
    struct Bomb;

    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("the payload was dropped");
        }
    }

    #[test]
    fn a_panic_payload_that_panics_when_dropped_does_not_escape_either() {
        // Given: a hook that panics with such a payload, before a recording one.
        let hooks = Arc::new(ConnectionHooks::default());
        hooks.register(Arc::new(|_, _| std::panic::panic_any(Bomb)));
        let seen = recording(&hooks);
        let connection = hooks.open(client("cli-a"));

        // When: the client leaves.
        drop(connection);

        // Then: the drop returned and the later hook ran.
        assert_eq!(*seen.lock().unwrap(), ["cli-a"]);
    }

    #[test]
    fn a_failing_hook_during_an_unwind_does_not_abort_the_process() {
        // Given: a hook that panics with a Bomb, and a connection held by code
        // that panics itself, so the guard drops while that panic unwinds (a
        // panic escaping the drop there would abort the whole test process).
        let hooks = Arc::new(ConnectionHooks::default());
        hooks.register(Arc::new(|_, _| std::panic::panic_any(Bomb)));
        let seen = recording(&hooks);

        // When: the holder panics.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _connection = hooks.open(client("cli-a"));
            panic!("the holder failed");
        }));

        // Then: only the holder's panic came out, and the later hook ran.
        assert!(outcome.is_err());
        assert_eq!(*seen.lock().unwrap(), ["cli-a"]);
    }

    #[test]
    fn a_later_connection_of_a_client_is_a_later_epoch_and_a_hook_gets_the_one_that_ended() {
        // Given: a hook that records the epoch it is told of.
        let hooks = Arc::new(ConnectionHooks::default());
        let ended = Arc::new(Mutex::new(Vec::new()));
        let sink = ended.clone();
        hooks.register(Arc::new(move |_, epoch| sink.lock().unwrap().push(epoch)));
        assert_eq!(hooks.epoch_of(&client("cli-a")), None);

        // When: two connections of the client share one epoch, and it comes back.
        let first = hooks.open(client("cli-a"));
        let second = hooks.open(client("cli-a"));
        let other = hooks.open(client("cli-b"));
        assert_eq!(first.epoch(), second.epoch());
        assert_eq!(hooks.epoch_of(&client("cli-a")), Some(first.epoch()));
        assert_ne!(first.epoch(), other.epoch());
        let before = first.epoch();
        drop((first, second));
        let again = hooks.open(client("cli-a"));

        // Then: it is a later epoch, and the hook was told the epoch that ended.
        assert!(again.epoch() > before);
        assert_eq!(*ended.lock().unwrap(), [before]);
    }

    #[test]
    fn a_hook_that_runs_after_the_client_came_back_still_names_the_epoch_that_ended() {
        // Given: a hook that, before it records, lets the client come back (the
        // notice reaches an area late).
        let hooks = Arc::new(ConnectionHooks::default());
        let inner = hooks.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let comeback = Arc::new(Mutex::new(None));
        let held = comeback.clone();
        hooks.register(Arc::new(move |client, epoch| {
            let again = inner.open(client.clone());
            sink.lock().unwrap().push((epoch, again.epoch()));
            *held.lock().unwrap() = Some(again);
        }));
        let connection = hooks.open(client("cli-a"));
        let ended = connection.epoch();

        // When: the client leaves.
        drop(connection);

        // Then: the epoch told is the one that ended, not the new one.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, ended);
        assert!(seen[0].1 > ended);
    }

    #[test]
    fn a_connection_without_a_hook_changes_nothing() {
        // Given: no hook at all.
        let hooks = Arc::new(ConnectionHooks::default());

        // When: a client connects and leaves.
        drop(hooks.open(client("cli-a")));

        // Then: nothing is left behind.
        assert_eq!(hooks.connections_of(&client("cli-a")), 0);
    }
}
