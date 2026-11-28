(async () => {
  const { EventStream } = await import("/js/sse.js");
  const { Api } = await import("/js/api.js");
  const { adminCommands, adminQueries, createAdminModel, isConflict, isDenied } = await import("/js/restart.js");
  const results = { passed: 0, failures: [] };
  async function test(name, fn) {
    try {
      await fn();
      results.passed += 1;
    } catch (error) {
      results.failures.push({ name, message: String(error?.message ?? error) });
    }
  }
  function check(condition, message) {
    if (!condition) throw new Error(message);
  }

  class FakeSource {
    static urls = [];
    static last = null;
    constructor(url) {
      FakeSource.urls.push(url);
      this.handlers = {};
      FakeSource.last = this;
    }
    addEventListener(name, handler) {
      this.handlers[name] = handler;
    }
    close() {}
    emit(name, data) {
      this.handlers[name]({ data: JSON.stringify(data) });
    }
  }
  function newStream() {
    FakeSource.urls = [];
    const timers = [];
    const stream = new EventStream({
      scope: "management",
      onStarted() {},
      onEvent() {},
      onStatus() {},
      EventSourceImpl: FakeSource,
      schedule: (fn, ms, label) => {
        timers.push({ fn, label });
        return timers.length;
      },
      cancel() {},
      random: () => 0,
    });
    const reconnect = () => timers.filter((t) => t.label === "retry").at(-1).fn();
    return { stream, reconnect };
  }
  const started = (extra) => ({ subscription_id: "s", high_water: { core_epoch: "7", sequence: "3" }, ...extra });

  await test("a replay that does not continue from the cursor drops the cursor", () => {
    const { stream, reconnect } = newStream();
    stream.start();
    FakeSource.last.emit("subscription_started", started({ snapshot_id: "snap" }));
    FakeSource.last.handlers.error();
    reconnect();
    check(FakeSource.urls.at(-1).includes("after=7%3A3"), `resume url ${FakeSource.urls.at(-1)}`);
    FakeSource.last.emit("subscription_started", started({ high_water: { core_epoch: "8", sequence: "0" } }));
    check(stream.last === null, "the cursor is dropped");
    reconnect();
    check(!FakeSource.urls.at(-1).includes("after="), `fresh url ${FakeSource.urls.at(-1)}`);
  });

  await test("an event of another epoch drops the cursor and asks for a snapshot", () => {
    const { stream, reconnect } = newStream();
    stream.start();
    FakeSource.last.emit("subscription_started", started({ snapshot_id: "snap" }));
    FakeSource.last.emit("ui_event", {
      subscription_id: "s",
      cursor: { core_epoch: "8", sequence: "1" },
      event: { type: "lifecycle", payload: { type: "admin_changed", payload: { revision: "1", restarting: false } } },
    });
    check(stream.last === null, "cursor kept after an epoch change");
    reconnect();
    check(!FakeSource.urls.at(-1).includes("after="), `url ${FakeSource.urls.at(-1)}`);
  });

  const targets = (epoch, revision) => ({
    core_epoch: epoch,
    revision,
    targets: [{ id: "current", revision }],
    truncated: false,
  });
  function openRestart(model) {
    return model.openConfirmation({ kind: "restart", key: "current", title: "t", lines: [], build: () => null });
  }

  await test("a changed target revision makes the confirmation stale", () => {
    const model = createAdminModel();
    model.setAccess("enabled");
    model.applyTargets(targets("2", "1"));
    openRestart(model);
    check(model.confirmationProblem() === null, "a fresh confirmation can be sent");
    model.applyTargets(targets("2", "2"));
    check(model.confirmationProblem() === "stale", `problem ${model.confirmationProblem()}`);
  });

  await test("a new core epoch voids the confirmation and drops what was held", () => {
    const model = createAdminModel();
    model.setAccess("enabled");
    model.applyTargets(targets("2", "1"));
    openRestart(model);
    model.applyTargets(targets("3", "1"));
    check(model.confirmationProblem() === "void", `problem ${model.confirmationProblem()}`);
    check(model.state.coreEpoch === "3", "the new epoch is held");
  });

  await test("a confirmation that was sent can only be read back, never sent again", () => {
    const model = createAdminModel();
    model.setAccess("enabled");
    model.applyTargets(targets("2", "1"));
    const c = openRestart(model);
    model.markSending(c, true);
    model.markSending(c, false);
    model.setOutcome(c, { type: "unknown", error: { code: "transport" } });
    check(model.confirmationProblem() === "sent", `problem ${model.confirmationProblem()}`);
  });

  await test("an answer for a closed confirmation does not touch the one now open", () => {
    const model = createAdminModel();
    model.setAccess("enabled");
    model.applyTargets(targets("2", "1"));
    const first = openRestart(model);
    model.markSending(first, true);
    model.closeConfirmation();
    const second = openRestart(model);
    check(model.setOutcome(first, { type: "accepted" }) === false, "the old answer was applied");
    model.markSending(first, false);
    check(second.outcome === null && second.sent === false, "the new confirmation changed");
    check(model.confirmationProblem() === null, "the new confirmation can still be sent");
  });

  await test("losing access drops every held binding", () => {
    const model = createAdminModel();
    model.setAccess("enabled");
    model.applyTargets(targets("2", "1"));
    openRestart(model);
    model.setAccess("disabled");
    check(model.state.targets.size === 0 && model.state.confirmation === null, "held state remains");
  });

  await test("no request carries a path, argv or environment", () => {
    const graph = {
      graph_id: "g1",
      restart_tx_id: "tx1",
      capture_id: "cap1",
      expected_revision: "1",
      expected_core_epoch: "2",
    };
    const binding = { graph, threads: [], expected_control_revision: "0" };
    const bodies = [
      adminQueries.targets(),
      adminQueries.status(),
      adminQueries.quarantineList(),
      adminQueries.undisposed(),
      adminQueries.ownedDetail("g1"),
      adminQueries.operationStatus("op-1"),
      adminCommands.requestRestart({ id: "current", revision: "1" }, "2", "op-1"),
      adminCommands.cancelRestart({ restart_id: "r1", revision: 4 }, "2", "op-2"),
      adminCommands.discard(graph, "op-3"),
      adminCommands.release(binding, "op-4"),
      adminCommands.resume(binding, "op-5"),
    ];
    const forbidden = new Set(["path", "argv", "env", "exe", "cmd", "command_line", "args", "cwd"]);
    const walk = (value) => {
      if (value && typeof value === "object") {
        for (const [key, inner] of Object.entries(value)) {
          check(!forbidden.has(key), `forbidden key ${key}`);
          walk(inner);
        }
      } else if (typeof value === "string") {
        check(!/[\\/]/.test(value), `path-like value ${value}`);
      }
    };
    // `kind: "command"` and the namespace names are fixed words, not input.
    bodies.forEach(walk);
  });

  await test("a management request is sent once, even when the answer is lost", async () => {
    let calls = 0;
    const api = new Api({
      fetchImpl: async () => {
        calls += 1;
        throw new Error("network down");
      },
    });
    const result = await api.adminRequest(adminCommands.requestRestart({ id: "current", revision: "1" }, "2", "op-1"));
    check(calls === 1, `${calls} sends`);
    check(!result.ok && result.error.code === "transport", "the loss is reported, not retried");
  });

  await test("conflict and denial are told apart", () => {
    check(isConflict({ code: "conflict" }) && isConflict({ code: "stale_revision" }), "conflict");
    check(isDenied({ code: "permission_denied" }) && !isDenied({ code: "conflict" }), "denied");
  });

  return results;
})()
