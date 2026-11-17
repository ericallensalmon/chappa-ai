// `restart <service>`: restart one service. The definition passes the
// row's name as `{row}`, so the Environment offers Restart on each row the
// status script reported, and the app checks the name before it gets here.
"use strict";

const s = require("./state");

const name = process.argv[2] || "";
if (!s.SERVICES.includes(name)) {
  console.error(`restart: \`${name}\` is not a service of this stack`);
  process.exit(2);
}
const state = s.load();
const svc = state && state.services[name];
if (!svc || !svc.running) {
  console.error(`${name}: not running; start it first`);
  process.exit(1);
}
svc.since = s.now();
s.save(state);
console.log(`${name}: restarted on port ${s.port(name)}`);
