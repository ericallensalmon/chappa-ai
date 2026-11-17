// `status`: print the stack's state as the status JSON the stack runner
// reads (one entry per service, plus facts). Nothing but JSON on stdout.
"use strict";

const s = require("./state");

const state = s.load();
// The groups the Environment shows, in this order; each entry names one.
const groups = [
  { id: "backend", label: "Back end" },
  { id: "frontend", label: "Front end" },
];
const groupOf = (name) => (name === "api" ? "backend" : "frontend");
const components = s.SERVICES.map((name) => {
  const svc = state && state.services[name];
  if (!svc) {
    return { name, group: groupOf(name), state: "not-deployed" };
  }
  // A real script would ask its engine and call the service; this one
  // pretends the service answers while it runs.
  const entry = {
    name,
    group: groupOf(name),
    source: { mode: "dev", ref: "main" },
    restarts: 0,
    probe: { url: `http://localhost:${s.port(name)}/`, rule: "any", status: svc.running ? 200 : null },
    state: svc.running ? "up" : "stopped-idle",
    since: svc.since,
    detail: svc.running ? `listening on port ${s.port(name)}` : "stopped",
  };
  if (svc.running) {
    entry.links = [{ label: `Open ${name}`, url: `http://localhost:${s.port(name)}` }];
  }
  return entry;
});
const facts = [{ label: "Instance", value: s.instance() }];
const api = s.worktree("api");
facts.push({ label: "api built from", value: api || "the shared checkout" });
console.log(JSON.stringify({ groups, components, facts, alerts: [] }));
