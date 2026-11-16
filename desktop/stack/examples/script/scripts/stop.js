// `stop`: stop both services, keeping them deployed (web first, then the
// api it talks to).
"use strict";

const s = require("./state");

const state = s.load();
if (!state) {
  console.log("nothing is deployed; nothing to stop");
  process.exit(0);
}
for (const name of [...s.SERVICES].reverse()) {
  const svc = state.services[name];
  if (svc && svc.running) {
    svc.running = false;
    svc.since = s.now();
    console.log(`${name}: stopped`);
  }
}
s.save(state);
