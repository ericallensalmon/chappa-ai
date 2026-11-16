// `up`: deploy both services and start them. Safe to run again: a
// service that already runs is left as it is.
"use strict";

const s = require("./state");

const state = s.load() || { services: {} };
for (const name of s.SERVICES) {
  const svc = state.services[name];
  if (svc && svc.running) {
    console.log(`${name}: already running on port ${s.port(name)}`);
    continue;
  }
  state.services[name] = { running: true, since: s.now() };
  console.log(`${name}: started on port ${s.port(name)}`);
}
const item = process.env.CHAPPA_STACK_ITEM;
console.log(item ? `built for work item ${item}` : "no work item bound; built from the shared checkouts");
s.save(state);
