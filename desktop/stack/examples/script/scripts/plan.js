// `plan <verb>`: say what <verb> would do, without doing it. Its output
// becomes the confirm of that verb.
"use strict";

const s = require("./state");

const verb = process.argv[2] || "";
const state = s.load();
const running = (name) => Boolean(state && state.services[name] && state.services[name].running);

switch (verb) {
  case "up":
    for (const name of s.SERVICES) {
      console.log(running(name) ? `${name}: already running, left as it is` : `${name}: start on port ${s.port(name)}`);
    }
    break;
  case "stop":
    for (const name of [...s.SERVICES].reverse()) {
      console.log(running(name) ? `${name}: stop` : `${name}: not running`);
    }
    break;
  case "start":
    if (!state) {
      console.log("nothing is deployed; start would fail (run up first)");
      break;
    }
    for (const name of s.SERVICES) {
      console.log(running(name) ? `${name}: already running` : `${name}: start`);
    }
    break;
  case "down":
    console.log(state ? `remove ${s.SERVICES.join(", ")}` : "nothing is deployed");
    break;
  default:
    console.error(`plan: unknown verb \`${verb}\``);
    process.exit(2);
}
