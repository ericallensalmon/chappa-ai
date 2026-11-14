// A stand-in `marker` hook for the example's database: a scrubbed image or
// scrub step would leave a marker (a table or a file) with its version and
// time, and the hook prints it. This one prints a fixed marker.
//
// A real hook reads the marker inside the container, for example
//   marker: [docker, exec, "{container}", cat, /var/lib/scrub-marker.json]
process.stdout.write(
  JSON.stringify({ scrubbed: true, version: "example-1", at: "2026-01-01T00:00:00Z" })
);
