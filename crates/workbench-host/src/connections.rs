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
//! * **Synchronous, short.** A hook runs in the connection's teardown, on the
//!   task that ends it. It must not wait: it only records a request that the
//!   area's own task carries out. No lock of this registry is held while a hook
//!   runs, so a hook may open or close connections, or register another hook.
//! * **No other effect.** Nothing here ends a run, or anything else, by
//!   itself: a connection that ends without an area that registered a hook
//!   changes nothing (a GUI that leaves does not stop its runs).
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use workbench_protocol::ids::ClientId;

/// What an area does when a client has no connection left.
pub type ConnectionClosedHook = Arc<dyn Fn(&ClientId) + Send + Sync>;

/// The open connections by client and the hooks to run when a client leaves.
#[derive(Default)]
pub struct ConnectionHooks {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    open: HashMap<ClientId, usize>,
    hooks: Vec<ConnectionClosedHook>,
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
        *lock(&self.state).open.entry(client.clone()).or_default() += 1;
        ClientConnection {
            hooks: self.clone(),
            client,
        }
    }

    /// How many connections `client` holds.
    pub fn connections_of(&self, client: &ClientId) -> usize {
        lock(&self.state).open.get(client).copied().unwrap_or(0)
    }

    fn closed(&self, client: &ClientId) {
        let hooks = {
            let mut state = lock(&self.state);
            let Some(count) = state.open.get_mut(client) else {
                return;
            };
            *count -= 1;
            if *count > 0 {
                return;
            }
            state.open.remove(client);
            state.hooks.clone()
        };
        for hook in hooks {
            hook(client);
        }
    }
}

/// One open connection of a client; dropping it ends the connection.
pub struct ClientConnection {
    hooks: Arc<ConnectionHooks>,
    client: ClientId,
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
        hooks.register(Arc::new(move |client| {
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
        hooks.register(Arc::new(move |client| {
            if client.as_str() == "cli-a" {
                sink.lock()
                    .unwrap()
                    .push(inner.open(ClientId::new("cli-c").unwrap()));
                inner.register(Arc::new(|_| {}));
            }
        }));

        // When: the client leaves.
        drop(hooks.open(client("cli-a")));

        // Then: the hook ran to its end and its connection is open.
        assert_eq!(hooks.connections_of(&client("cli-c")), 1);
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
