//! Copies made in a preview reach the host clipboard; the page never reads it.
//!
//! Headless CEF keeps its own in-memory clipboard, so a hook installed in each
//! page context reports text the user copied (`copy`/`cut` events and
//! `navigator.clipboard.writeText`, both only with user activation) to the
//! browser process, and the view writes it to a [`Clipboard`].

/// Where copied text goes; the view uses egui's clipboard unless given another.
pub trait Clipboard: Send + Sync {
    fn write_text(&self, text: String);
}

impl Clipboard for egui::Context {
    fn write_text(&self, text: String) {
        self.copy_text(text);
    }
}

/// Process message the hook sends with the copied text as its only argument.
pub const COPY_MESSAGE: &str = "evorch.copy";

/// Larger copies are dropped rather than forwarded.
pub const MAX_COPY_BYTES: usize = 1 << 20;

/// Copies waiting for the view; older ones are dropped beyond this.
pub const MAX_PENDING_COPIES: usize = 16;

/// Evaluates to a function that installs the hook given the native `send`.
/// It runs before page scripts and captures every built-in it relies on, so
/// later page changes cannot redirect it. `send` stays in the closure and is
/// never reachable from the page.
pub const COPY_HOOK: &str = r#"(function (send) {
  "use strict";
  const activation = navigator.userActivation;
  const isActive = Object.getOwnPropertyDescriptor(UserActivation.prototype, "isActive").get;
  const activeElement = Object.getOwnPropertyDescriptor(Document.prototype, "activeElement").get;
  const getSelection = window.getSelection;
  const schedule = window.setTimeout;
  const setData = DataTransfer.prototype.setData;
  const active = () => isActive.call(activation);
  let copying = null;

  const fieldSelection = (element) => {
    try {
      const start = element.selectionStart;
      const end = element.selectionEnd;
      if (typeof start === "number" && typeof end === "number" && element.type !== "password") {
        return String(element.value).slice(start, end);
      }
    } catch (_) {}
    return null;
  };
  const selected = () => {
    const element = activeElement.call(document);
    const field = element ? fieldSelection(element) : null;
    return field !== null ? field : String(getSelection.call(window));
  };

  DataTransfer.prototype.setData = function (format, data) {
    if (copying !== null && this === copying.data && /^text(\/plain)?$/i.test(String(format))) {
      copying.text = String(data);
    }
    return setData.call(this, format, data);
  };

  const onCopy = (event) => {
    if (!event.isTrusted || !active()) {
      return;
    }
    const state = { data: event.clipboardData, text: null, selection: selected() };
    copying = state;
    schedule.call(window, () => {
      if (copying === state) {
        copying = null;
      }
      const text = event.defaultPrevented ? state.text : state.selection;
      if (text) {
        send(text);
      }
    }, 0);
  };
  window.addEventListener("copy", onCopy, true);
  window.addEventListener("cut", onCopy, true);

  // `Clipboard` itself is not exposed yet when this runs; its prototype is.
  const clipboard = navigator.clipboard ? Object.getPrototypeOf(navigator.clipboard) : null;
  if (clipboard) {
    const denied = (message) => Promise.reject(new DOMException(message, "NotAllowedError"));
    clipboard.writeText = function (text) {
      if (!active()) {
        return denied("Writing to the clipboard needs a user gesture");
      }
      send(String(text));
      return Promise.resolve();
    };
    clipboard.readText = function () {
      return denied("HTML previews cannot read the clipboard");
    };
    clipboard.read = function () {
      return denied("HTML previews cannot read the clipboard");
    };
  }
})"#;
