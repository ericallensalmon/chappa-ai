// `status`: print the stack's state as the status JSON the stack runner
// reads (one entry per service, plus facts). Nothing but JSON on stdout.
"use strict";

const s = require("./state");

const state = s.load();
const components = s.SERVICES.map((name) => {
  const svc = state && state.services[name];
  if (!svc) {
    return { name, state: "not-deployed" };
  }
  const entry = {
    name,
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
console.log(JSON.stringify({ components, facts, alerts: [] }));
