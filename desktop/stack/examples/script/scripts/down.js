// `down`: stop and remove both services.
"use strict";

const s = require("./state");

if (s.load()) {
  console.log(`removing ${s.SERVICES.join(", ")}`);
} else {
  console.log("nothing is deployed");
}
s.remove();
