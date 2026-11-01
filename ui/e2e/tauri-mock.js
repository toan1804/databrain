// A stand-in for the Tauri backend, injected into the page before the app
// loads (Playwright addInitScript). The app's `invoke` calls land here:
// answers come from `window.__DB_MOCK_DATA__` (set by the test before this
// script runs; a value, or for stateful commands a built-in handler below),
// every call is recorded in `window.__DB_MOCK__.calls`, and tests push
// backend events with `window.__DB_MOCK__.emit(event, payload)`.
(() => {
  const data = window.__DB_MOCK_DATA__ || {};
  const callbacks = new Map();
  const listeners = new Map();
  let nextId = 1;
  const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

  const mock = {
    calls: [],
    /** Responses by command: a value or `(args, mock) => value` (set from the page). */
    handlers: {},
    /** In-memory tables the built-in handlers keep (jobs, settings). */
    db: { jobs: clone(data.__jobs ?? []), settings: clone(data.get_settings ?? {}) },
    emit(event, payload) {
      for (const id of listeners.get(event) ?? []) callbacks.get(id)?.({ event, id, payload });
    },
    callsOf(cmd) {
      return mock.calls.filter((c) => c.cmd === cmd);
    },
  };
  window.__DB_MOCK__ = mock;

  const builtins = {
    get_settings: () => mock.db.settings,
    set_setting: (a) => {
      mock.db.settings[a.key] = a.value;
      return null;
    },
    list_jobs: () => ({
      jobs: mock.db.jobs.map((j) => ({
        id: j.id,
        name: j.name,
        node_count: j.nodes.length,
        step_names: j.nodes.map((n) => n.name),
        schedule: j.schedule,
        next_run_at: null,
        last_run: null,
        updated_at: j.updated_at,
      })),
      running: [],
      elsewhere: [],
    }),
    get_job: (a) => mock.db.jobs.find((j) => j.id === a.id) ?? { __error: { kind: "not_found", message: `job ${a.id} not found` } },
    save_job: (a) => {
      const job = { ...a.job, id: a.job.id || `job-${nextId++}`, updated_at: Date.now(), created_at: a.job.created_at || Date.now() };
      mock.db.jobs = [...mock.db.jobs.filter((j) => j.id !== job.id), job];
      return job;
    },
    delete_job: (a) => {
      mock.db.jobs = mock.db.jobs.filter((j) => j.id !== a.id);
      return null;
    },
  };

  function answer(cmd, args) {
    if (cmd in mock.handlers) {
      const h = mock.handlers[cmd];
      return typeof h === "function" ? h(args, mock) : clone(h);
    }
    if (cmd in data && !cmd.startsWith("__")) return clone(data[cmd]);
    if (cmd in builtins) return builtins[cmd](args);
    // Lists are empty, everything else is "nothing".
    if (/^(list_|load_tabs$|ai_list_|ai_sessions$|ai_session_messages$|complete_tables|complete_routines|job_runs$|job_run_log$|search_)/.test(cmd)) return [];
    return null;
  }

  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener(event, id) {
      listeners.set(event, (listeners.get(event) ?? []).filter((x) => x !== id));
    },
  };
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
    plugins: {},
    transformCallback(cb, once = false) {
      const id = nextId++;
      callbacks.set(id, (d) => {
        if (once) callbacks.delete(id);
        return cb?.(d);
      });
      return id;
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
    convertFileSrc: (p) => p,
    async invoke(cmd, args = {}) {
      if (cmd === "plugin:event|listen") {
        listeners.set(args.event, [...(listeners.get(args.event) ?? []), args.handler]);
        return args.handler;
      }
      if (cmd === "plugin:event|unlisten" || cmd === "plugin:event|emit") return null;
      mock.calls.push({ cmd, args: clone(args) });
      const r = answer(cmd, args);
      if (r && typeof r === "object" && r.__error) throw r.__error;
      return r;
    },
  };
})();
