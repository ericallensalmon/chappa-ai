// `start`: start what is deployed, without deploying anything new.
"use strict";

const s = require("./state");

const state = s.load();
if (!state) {
  console.error("nothing is deployed; run up first");
  process.exit(1);
}
for (const name of s.SERVICES) {
  const svc = state.services[name];
  if (svc && !svc.running) {
    svc.running = true;
    svc.since = s.now();
    console.log(`${name}: started on port ${s.port(name)}`);
  }
}
s.save(state);
