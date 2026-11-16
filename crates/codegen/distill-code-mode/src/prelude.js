(() => {
  const hostCall = globalThis.__distill_host_call;
  delete globalThis.__distill_host_call;

  const pending = new Map();
  const logs = [];
  let nextId = 0;

  const describe = (value) => {
    if (value instanceof Error) return `${value.name}: ${value.message}`;
    if (typeof value === "string") return value;
    try {
      const json = JSON.stringify(value);
      return json === undefined ? String(value) : json;
    } catch {
      return String(value);
    }
  };

  const render = (value) => {
    if (typeof value === "string") return value;
    try {
      const json = JSON.stringify(value, null, 2);
      return json === undefined ? String(value) : json;
    } catch {
      return String(value);
    }
  };

  const console = {};
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    console[level] = (...args) => {
      logs.push(args.map(describe).join(" "));
    };
  }
  globalThis.console = console;

  globalThis.tools = new Proxy(
    {},
    {
      get: (_, name) =>
        typeof name !== "string"
          ? undefined
          : (args) =>
              new Promise((resolve, reject) => {
                const id = ++nextId;
                pending.set(id, { resolve, reject });
                try {
                  hostCall(id, name, JSON.stringify(args === undefined ? {} : args));
                } catch (error) {
                  pending.delete(id);
                  reject(error);
                }
              }),
    },
  );

  return {
    settle(id, ok, payload) {
      const entry = pending.get(id);
      if (!entry) return;
      pending.delete(id);
      if (ok) entry.resolve(payload);
      else entry.reject(new Error(payload));
    },
    logs() {
      return logs.join("\n");
    },
    run(main) {
      return main().then(
        (value) => JSON.stringify({ ok: true, value: value === undefined ? null : render(value) }),
        (error) => JSON.stringify({ ok: false, error: describe(error) }),
      );
    },
  };
})()
