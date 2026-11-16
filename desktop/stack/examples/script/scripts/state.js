// The pretend stack's state, shared by every script: one JSON file per
// instance under .work/fake/, saying which services exist and whether
// they run. A real stack script would ask its engine instead.
"use strict";

const fs = require("fs");
const path = require("path");

const SERVICES = ["api", "web"];

function instance() {
  return process.env.CHAPPA_STACK_INSTANCE || "scripted-0";
}

function file() {
  return path.join(process.cwd(), ".work", "fake", `${instance()}.json`);
}

/** The state, or null when the stack is not deployed. */
function load() {
  try {
    return JSON.parse(fs.readFileSync(file(), "utf8"));
  } catch {
    return null;
  }
}

function save(state) {
  fs.mkdirSync(path.dirname(file()), { recursive: true });
  fs.writeFileSync(file(), JSON.stringify(state, null, 2));
}

function remove() {
  fs.rmSync(file(), { force: true });
}

function now() {
  return new Date().toISOString().replace(/\.\d+Z$/, "Z");
}

/** The host port of a service in this slot, from the instance environment. */
function port(service) {
  const key = service === "api" ? "CHAPPA_STACK_APP_PORT" : "CHAPPA_STACK_APP_PORT_1";
  return process.env[key] || "?";
}

/** Where the bound work item's checkout of a repo is, if one is bound. */
function worktree(repo) {
  return process.env[`CHAPPA_STACK_WORKTREE_${repo.toUpperCase()}`] || null;
}

module.exports = { SERVICES, instance, load, save, remove, now, port, worktree };
