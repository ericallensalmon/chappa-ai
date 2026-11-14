// A stand-in `sample` hook for the example's database: it prints the JSON
// rows a real hook would print after querying a bounded sample, without
// touching the database. Every value is a test fixture: documentation
// domains (example.com/.org) and no card numbers, so the check is clean.
//
// A real hook runs the query inside the container, for example
//   sample: [docker, exec, "{container}", psql, -U, app, -d, app, -At, -c, "<query>"]
// with a query that prints this same shape.
//
// To see a finding, add the published test card number
// "4111 1111 1111 1111" to the `card` values below and run the check again.
const rows = [
  {
    table: "customers",
    column: "email",
    values: ["user1@example.com", "user2@example.org", "user3@example.com"],
  },
  {
    table: "customers",
    column: "name",
    values: ["Customer 1", "Customer 2", "Customer 3"],
  },
  {
    table: "payments",
    column: "card",
    values: ["TEST-CARD-1", "TEST-CARD-2"],
  },
];
process.stdout.write(JSON.stringify(rows));
