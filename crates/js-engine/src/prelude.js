// Evaluated once per run before the user code (ADR 0039). It evaluates to the
// driver object; nothing is left on the global object except the documented
// user-facing API (`tools`, `readText`, `console`, `print`).
(function (g) {
  "use strict";
  const hostCall = g.__hostCall;
  const hostPrint = g.__hostPrint;
  const toolNames = g.__toolNames;
  delete g.__hostCall;
  delete g.__hostPrint;
  delete g.__toolNames;

  const READ_TEXT_LIMIT = 4 * 1024 * 1024;

  function fmt(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return value.stack ? String(value.stack) : String(value);
    if (typeof value === "object" && value !== null) {
      try {
        const json = JSON.stringify(value);
        if (json !== undefined) return json;
      } catch (_) {
        // fall through to String()
      }
    }
    try {
      return String(value);
    } catch (_) {
      return "[unprintable]";
    }
  }

  function write(...args) {
    if (!hostPrint(args.map(fmt).join(" ") + "\n")) {
      throw new Error("run_js output limit exceeded");
    }
  }

  const consoleObject = { log: write, info: write, debug: write, warn: write, error: write };
  Object.defineProperty(g, "console", { value: consoleObject, writable: true, configurable: true });
  Object.defineProperty(g, "print", { value: write, writable: true, configurable: true });

  // The stack of a rejection would end in the host callback; the call site is
  // captured synchronously so the error still points at the user's line.
  function call(name, args) {
    const site = new Error().stack;
    const raw = JSON.stringify(args === undefined ? {} : args);
    return hostCall(name, raw === undefined ? "{}" : raw).then((text) => {
      const reply = JSON.parse(text);
      if (!reply.ok) {
        const error = new Error(reply.error);
        error.stack = "Error: " + reply.error + "\n" + site;
        throw error;
      }
      return reply.value;
    });
  }

  const tools = {};
  for (const name of toolNames) {
    tools[name] = (args) => call(name, args);
  }
  Object.freeze(tools);
  Object.defineProperty(g, "tools", { value: tools, writable: false, configurable: false });

  // The limit counts what the file occupies, not UTF-16 units.
  function utf8Length(value) {
    let length = 0;
    for (const character of value) {
      const point = character.codePointAt(0);
      length += point < 0x80 ? 1 : point < 0x800 ? 2 : point < 0x10000 ? 3 : 4;
    }
    return length;
  }

  async function readText(path) {
    if (typeof tools.read !== "function") throw new Error("readText needs tools.read, which is not available");
    let args = { path };
    let text = "";
    let bytes = 0;
    for (;;) {
      const result = await tools.read(args);
      if (!result.ok) throw new Error(result.text);
      text += result.text;
      bytes += utf8Length(result.text);
      if (bytes > READ_TEXT_LIMIT) {
        throw new Error("readText: " + path + " exceeds the 4 MiB limit");
      }
      if (!result.next) return text;
      args = { path: result.next.path };
      if (result.next.offset != null) args.offset = result.next.offset;
      if (result.next.byte_offset != null) args.byte_offset = result.next.byte_offset;
    }
  }
  Object.defineProperty(g, "readText", { value: readText, writable: false, configurable: false });

  function userStack(error) {
    if (error === null || typeof error !== "object" || typeof error.stack !== "string") return "";
    return error.stack
      .split("\n")
      .filter((line) => !line.includes("<prelude>"))
      .join("\n");
  }

  return {
    async run(main) {
      try {
        const value = await main();
        return JSON.stringify({ ok: true, value });
      } catch (error) {
        let message;
        try {
          message =
            error !== null && typeof error === "object" && typeof error.message === "string"
              ? (error.name ? error.name + ": " : "") + error.message
              : fmt(error);
        } catch (_) {
          message = "[unprintable error]";
        }
        return JSON.stringify({ ok: false, message, stack: userStack(error) });
      }
    },
  };
})(globalThis);
