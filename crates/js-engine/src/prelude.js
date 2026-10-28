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

  async function call(name, args) {
    const raw = JSON.stringify(args === undefined ? {} : args);
    const reply = JSON.parse(await hostCall(name, raw === undefined ? "{}" : raw));
    if (!reply.ok) throw new Error(reply.error);
    return reply.value;
  }

  const tools = {};
  for (const name of toolNames) {
    tools[name] = (args) => call(name, args);
  }
  Object.freeze(tools);
  Object.defineProperty(g, "tools", { value: tools, writable: false, configurable: false });

  async function readText(path) {
    if (typeof tools.read !== "function") throw new Error("readText needs tools.read, which is not available");
    let args = { path };
    let text = "";
    for (;;) {
      const result = await tools.read(args);
      if (!result.ok) throw new Error(result.text);
      text += result.text;
      if (text.length > READ_TEXT_LIMIT) {
        throw new Error("readText: " + path + " exceeds the 4 MiB limit");
      }
      if (!result.next) return text;
      args = { path: result.next.path };
      if (result.next.offset !== undefined) args.offset = result.next.offset;
      if (result.next.byte_offset !== undefined) args.byte_offset = result.next.byte_offset;
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
