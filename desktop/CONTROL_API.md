# chappa-ai control API

GENERATED from `desktop/src-tauri/src/control_http.rs::ROUTES` — do not edit by hand.
Regenerate: `CONTROL_API_WRITE=1 cargo test -p chappa-ai-desktop control_api_doc`.

Bound to `http://127.0.0.1:8324` in every build. Every request must carry
`Authorization: Bearer <token>` where the token is the contents of
`<app-config>/control_token` (created at first launch, owner-only permissions;
`<app-config>` is Tauri's app config dir for `com.chappa-ai.desktop`, e.g.
`%APPDATA%\com.chappa-ai.desktop` on Windows). Responses are JSON; errors are
`{"error": "…"}` with a 4xx/5xx status. `GET /` returns this same table.

Path placeholders: `{id}` = terminal id, `{project}` = project id, `{name}` =
percent-encoded process name, `{rid}` = scratchpad / todo / comment id.
Fields marked `?` are optional; POST fields are JSON body keys, GET fields are
query parameters (comma-separated for list fields).

This is the MCP server's transport (`chappa-ai-mcp`) and a curl interface —
not a public API. Localhost only, no CLI binary.

| Method | Path | Fields | Description |
|---|---|---|---|
| `GET` | `/` | — | This route table as JSON (discoverability), plus `instance` {pid, config_dir} — which running chappa-ai answered, so two instances (or a stale one holding the port) are diagnosable. |
| `GET` | `/processes` | — | Every live terminal/process: id, name, status, exit_code, cols, rows, seq, plus byte-stream liveness (output_bytes, last_output_at, has_output, child_alive), `close_on_exit` (the spawn flag — the row reaps itself when its child exits) and, for agents, `agent.parent_process_id` (the process that spawned it, null for a root). Stats fields will ride along here. |
| `POST` | `/processes` | `command?`, `args?`, `cwd?`, `name?`, `cols?`, `rows?`, `env?` | Spawn a terminal. `command` + `args` reach the OS verbatim (never re-split); no command = the default shell; `env` = {NAME: value} applied over the inherited environment. cols/rows 1..=4096. Returns {id, name}. |
| `GET` | `/processes/{id}` | — | One process row (same shape as the listing). has_output=false means still booting; child_alive=false means the child has exited. |
| `GET` | `/processes/{id}/output` | `lines?`, `offset?` | Terminal text. Default = the WHOLE current screen. `lines`=N adds the last N scrollback lines above it; `offset`=O (with lines) returns a history-only window skipping the O most recent scrollback lines. No silent cap. |
| `GET` | `/processes/{id}/input_journal` | — | The last 100 send_input/send_bytes deliveries: [{ts, bytes_len, text, submit}], oldest first. |
| `POST` | `/processes/{id}/input` | `text`, `submit?`, `wait_ms?` | Write text to the pty in ONE atomic write (submit appends \r to the same write). Returns a receipt {written, seq_before, seq_after, had_output_within_ms, wait_ms} off the byte counter. For a json-transport agent the text is delivered as ONE user message (claude: a stream-json user line on stdin; opencode: a `run -s <session>` continuation child) and the receipt is {written, transport: "json", delivered, reason?, waited_ms, seq_before, turn_started_seq?, wait_ms}: delivered=true means the CLI's next turn_started arrived within wait_ms (default 5000, max 20000); reason=timeout means written but not yet acknowledged, exited = no child to write to (or it died before its turn started: `exited: <why>`), busy = the previous opencode turn is still running. `wait_ms` is the effective wait. A json agent takes whole messages: `submit: false` or empty text is a 400, and a refused send (busy/exited/unsupported) is not journaled. |
| `GET` | `/processes/{id}/agent_events` | `since?` | A json-transport agent's typed events with seq > since (default 0), from a per-agent ring of the last 1000: [{id, seq, ts, kind, payload}], kind = turn_started \| turn_ended \| tool_call \| text \| usage \| awaiting_input \| compaction \| error \| raw. `raw` carries the unclassified native object (or {line} / {stderr}) so nothing is lost. 409 for a tty agent or a plain terminal. |
| `POST` | `/processes/{id}/bytes` | `bytes`, `wait_ms?` | Raw bytes ([u8]) to the pty, same receipt and journal treatment as /input. The escape hatch for control sequences. |
| `POST` | `/processes/{id}/close` | `confirm` | Close a terminal (kills the child). Requires confirm=true. For a docker-exec agent the container-side process is killed and verified FIRST (TERM, poll, KILL), then the host client; the answer carries verification = gone \| still-present \| container-down \| unresolved (null for plain terminals). |
| `GET` | `/agent_tools` | — | Registered agent tools (the shape {id, name, command, tool_type, enabled} plus model, runtime {kind: host \| docker_exec, container?, user?, workdir?, tty?, max_busy_in_container?}, container (the tool's container name, or null), program, args, env, max_busy). `command` is DERIVED for display; program/args are the truth. |
| `POST` | `/agents` | `agent_tool_id`, `project_id?`, `name?`, `extra_args?`, `prompt?`, `force?`, `cols?`, `rows?`, `parent_process_id?`, `close_on_exit?` | Spawn an agent from a registered tool. extra_args are appended to the tool's args VERBATIM (an element with spaces stays one argv entry). Env = inherited < tool.env < identity (CHAPPA_AI_PROCESS_ID/PROJECT_ID/AGENT_TOOL_ID); docker-exec runtimes pass every env entry as -e and run the program under a pid-recording sh wrapper. `prompt` queues until the pty is ready (has_output and agent_ready_quiet_ms of silence, default 750 ms — or agent_ready_max_wait_ms after the first byte for a TUI that never goes quiet, reason=ready-by-timeout), then ONE atomic write of text+\r; prompt_receipt reports delivered=false with reason=exited when the child dies first. Busy guards: over max_busy / max_busy_in_container the spawn is refused naming the busy processes; force=true overrides. Returns {process_id, term_id, name, agent_instructions, prompt_receipt?, container? (status, started_at, uptime_s, mem_bytes, mem_limit_bytes), agent, forced?}. The row NESTS under its parent in the rail — `parent_process_id` if given, else the `X-Chappa-Actor` header parsed as a process id (chappa-ai-mcp sends its own CHAPPA_AI_PROCESS_ID, so an agent spawning an agent nests automatically); the candidate must name a live process or the spawn records null (a root) — a hint for the rail, never authorization. `close_on_exit=true` closes the row through the ordinary close path once the child exits (after term://exited and the exit notification; non-zero exits close too) and broadcasts `term://closed {term_id, reason: "close_on_exit"}`; every other close broadcasts `reason: "closed"`. A bash-hosted tool never exits by itself: end its command with `; exit`. |
| `POST` | `/agents/reap` | — | Sweep every ENABLED docker_exec tool's distinct (container, user) pair for container processes carrying CHAPPA_AI_SPAWN_UUID that no LIVE registry entry owns, and reap each orphan root through the shared TERM/poll/KILL/verify sequence (as the tool's user). Returns one entry per orphan handled: {container, user, pid, uuid, verdict: gone \| still-present \| container-down \| unresolved}. NOT gated by the agentReapOrphans setting (that gates only the automatic startup/probe sweeps) - this is the on-demand path. A container that is down or docker unreachable is skipped silently. Does not touch processes without the marker, and never consults docker top. |
| `GET` | `/projects` | — | Stored projects: [{id, name, path, icon, open, processes}]; processes are populated only for open projects. |
| `GET` | `/projects/{project}` | — | One project with its process rows (status, term_id, exit_code, auto_start, …). |
| `POST` | `/projects/{project}/processes/{name}/start` | `wait_ms?` | Start a project process (idempotent on a live one). The app's frontend performs the spawn; waits up to wait_ms (default 3000) for it to register, else answers pending=true. |
| `POST` | `/projects/{project}/processes/{name}/stop` | — | Stop a project process. Non-running = no-op success (never an error). |
| `POST` | `/projects/{project}/processes/{name}/restart` | `wait_ms?` | Restart = stop (no-op if not running) then start. |
| `GET` | `/scratchpads` | `project_id?`, `include_global?`, `query?`, `tags?`, `include_archived?`, `offset?`, `limit?` | List scratchpads (no content): {scratchpads: [{scratchpad_id, project_id, name, revision, tags, archived, updated_at, updated_by, line_count, matched_fields?, snippet?}], total}. `project_id` scopes to one project (`global` = pads with no project; absent = every pad); include_global=true adds the global pads to a project scope; `query` matches name + content case-insensitively and adds matched_fields + a snippet; `tags` (comma list) = any-of; archived pads are hidden unless include_archived=true. |
| `POST` | `/scratchpads` | `name`, `content`, `tags?`, `project_id?`, `response_mode?` | Create a scratchpad (scratchpad_write without scratchpad_id). A leading `# Title` line in the content overrides `name`. project_id null/absent = a global pad. Answers a slim receipt {scratchpad_id, project_id, revision, name}; response_mode=rich answers the full row. |
| `GET` | `/scratchpads/tags` | `project_id?` | Distinct scratchpad tags: {tags: [..]} (project_id scopes; absent = every pad). |
| `GET` | `/scratchpads/{rid}` | `mode?`, `section_heading?`, `offset?`, `limit?` | Read a scratchpad with its revision metadata. mode = full (default; `offset`/`limit` window the lines) \| outline (heading outline only, no content) \| section (one markdown section — `section_heading` required, exact text after trimming, first duplicate wins). The `content`/`headings`/`lines` spellings are accepted. |
| `POST` | `/scratchpads/{rid}` | `name`, `content`, `expected_revision`, `tags?`, `response_mode?` | Overwrite a scratchpad (scratchpad_write with scratchpad_id). expected_revision is REQUIRED; a mismatch is 409 {error: revision_conflict, current_revision, current_content}. A leading `# Title` overrides name; omitted tags are kept. |
| `GET` | `/scratchpads/{rid}/find` | `query`, `case_sensitive?`, `limit?`, `context_lines?`, `scope?` | Literal substring search in one pad: {matches: [{line (1-based), text, before, after}], total_matches, truncated}. limit default 20 (1..=100), context_lines default 1 (0..=3), scope = all \| headings \| content. |
| `GET` | `/scratchpads/{rid}/tail` | `lines?` | The last `lines` lines (default 10) with revision metadata. |
| `POST` | `/scratchpads/{rid}/rename` | `name`, `expected_revision`, `response_mode?` | Rename at expected_revision (required) without touching the content. |
| `POST` | `/scratchpads/{rid}/append` | `content`, `expected_revision?`, `response_mode?` | Append at the end (a newline separates it from existing content). expected_revision is optional — appends cannot clobber, last-writer-wins is fine; the revision still advances. |
| `POST` | `/scratchpads/{rid}/append_section` | `heading`, `content`, `expected_revision?`, `response_mode?` | Append at the end of the section under an EXISTING heading (before the next heading of the same or higher level). A missing heading is 400 and changes nothing. expected_revision optional. |
| `POST` | `/scratchpads/{rid}/edit` | `target`, `content`, `expected_revision`, `response_mode?` | Replace one section or line range at expected_revision (required). target = {heading} (content starting with a heading replaces the whole section; otherwise the heading is kept and its body replaced) \| {start_line, end_line} (1-based inclusive, bounds-checked). {type: section, section_heading} / {type: line_range, offset, limit} are accepted too. Never renames: the leading-H1 title override applies on create and full overwrite only, so an explicit rename survives edits. |
| `POST` | `/scratchpads/{rid}/tags/add` | `tags`, `expected_revision`, `response_mode?` | Add tags in one revision bump at expected_revision (required). |
| `POST` | `/scratchpads/{rid}/tags/remove` | `tags`, `expected_revision`, `response_mode?` | Remove tags in one revision bump at expected_revision (required). |
| `POST` | `/scratchpads/{rid}/clear` | `expected_revision`, `confirm`, `response_mode?` | Empty the content at expected_revision (required). Requires confirm=true. |
| `POST` | `/scratchpads/{rid}/delete` | `expected_revision`, `confirm` | Delete the pad at expected_revision (required). Requires confirm=true. Answers {scratchpad_id, project_id, deleted: true}. |
| `POST` | `/scratchpads/{rid}/archive` | `archived?`, `response_mode?` | Hide from the default list without deleting (archived=false un-hides). No revision guard. |
| `POST` | `/scratchpads/{rid}/transfer` | `target_project_id`, `expected_revision?`, `response_mode?` | Move the pad to another project (null = global). Only the project changes. |
| `GET` | `/todos` | `project_id?`, `status?`, `completed?`, `is_blocked?`, `priority?`, `query?`, `tags?`, `sort?`, `offset?`, `limit?` | Todo summaries (no body): {todos: [..], total}. Filters: status (open \| in_progress \| done), completed, is_blocked (derived: any blocker not completed), priority (low \| normal \| high \| urgent), query (title + body + comments), tags (any-of). sort = updated_desc (default) \| updated_asc \| created_desc \| created_asc \| priority_desc \| priority_asc \| title. limit default 50, max 200. |
| `POST` | `/todos` | `project_id`, `title`, `body?`, `priority?`, `tags?`, `response_mode?` | Create a todo in a project (project_id required — todos are project-scoped). Slim receipt {todo_id, project_id, revision, title}; response_mode=rich answers the full row. |
| `GET` | `/todos/tags` | `project_id?` | Distinct todo tags: {tags: [..]}. |
| `GET` | `/todos/{rid}` | `include_comments?` | One todo: {todo_id, project_id, title, body, priority, status, completed, tags, revision, locked_by, lock_expires_at, is_blocked, blocker_ids, comment_count, created_at, updated_at, updated_by} (+ comments with include_comments=true). An expired lock reads as no lock. |
| `POST` | `/todos/{rid}` | `title?`, `body?`, `priority?`, `status?`, `tags?`, `expected_revision?`, `response_mode?` | Update a subset of fields; omitted fields are preserved. status=done sets completed. A foreign live lock is 409 {error: locked, locked_by, lock_expires_at}; a stale expected_revision is 409 revision_conflict. Slim receipt = {todo_id, project_id, revision} + the changed fields. |
| `POST` | `/todos/{rid}/tags/add` | `tag`, `response_mode?` | Add one tag (lock-guarded). |
| `POST` | `/todos/{rid}/tags/remove` | `tag`, `response_mode?` | Remove one tag (lock-guarded). |
| `POST` | `/todos/{rid}/blockers/set` | `blocker_ids`, `response_mode?` | Replace the blocker list. Blockers must be in the same project; a cycle (including self) is 409 {error: cycle}. Answers blocker_ids + the derived is_blocked. |
| `POST` | `/todos/{rid}/blockers/add` | `blocker_id`, `response_mode?` | Add one blocker (cycle-checked). |
| `POST` | `/todos/{rid}/blockers/remove` | `blocker_id`, `response_mode?` | Remove one blocker. |
| `POST` | `/todos/{rid}/complete` | `completed`, `release_lock?`, `response_mode?` | Mark complete (status=done) or incomplete (status back to open). Releases the caller's OWN lock unless release_lock=false. affected_todo_ids = todos blocked by this one. |
| `POST` | `/todos/{rid}/lock` | `lease_ms?`, `lease_ttl_seconds?`, `response_mode?` | Take or renew an edit lease keyed by ACTOR id (default 300 s, max 24 h; lease_ttl_seconds saturates). A foreign live lock is 409 locked; expired leases are ignored. Advisory coordination only: the actor is the unauthenticated X-Chappa-Actor header, so a lock is a courtesy between cooperating agents, not access control. |
| `POST` | `/todos/{rid}/unlock` | `response_mode?` | Release a lock you hold (none/expired = no-op success; a foreign live lock is 409). |
| `POST` | `/todos/{rid}/transfer` | `target_project_id`, `response_mode?` | Move a todo to another project: comments kept, blockers (both directions) and lock cleared. |
| `POST` | `/todos/{rid}/delete` | `confirm` | Delete a todo with its comments and blocker edges. Requires confirm=true. Answers {todo_id, project_id, deleted, affected_todo_ids}. |
| `GET` | `/todos/{rid}/comments` | `offset?`, `limit?` | Comments on a todo, oldest first: {comments: [{comment_id, todo_id, actor, body, created_at, updated_at}], total}. |
| `POST` | `/todos/{rid}/comments` | `body`, `response_mode?` | Add a comment (attributed to the actor). Slim receipt {comment_id, todo_id}. |
| `POST` | `/todo_comments/{rid}` | `body`, `response_mode?` | Edit a comment's body. |
| `POST` | `/todo_comments/{rid}/delete` | `confirm` | Delete a comment. Requires confirm=true. |
| `POST` | `/timers` | `delay_ms`, `body`, `delivery_process_id?`, `loop?`, `repeat_every_ms?`, `name?`, `project_id?` | timer_set: fire `body` into a process after delay_ms. `loop: true` repeats every delay_ms; `repeat_every_ms` overrides the interval. delivery_process_id defaults to the CALLER's own process (the X-Chappa-Actor header, which chappa-ai-mcp fills from CHAPPA_AI_PROCESS_ID) — a timer delivers to exactly one process, which must be LIVE now (404 otherwise) and is pinned by its uuid. project_id defaults to the actor's own project. The body is injected verbatim as a fresh user turn through the delivery path (quiet-only policy). |
| `POST` | `/timers/idle_any` | `processes`, `max_wait_ms`, `body`, `delivery_process_id?`, `idle_ms?`, `confirm_ms?`, `rearm?`, `name?`, `project_id?` | timer_fire_when_idle_any: fire when ANY watched process goes idle, or at max_wait_ms (reason=deadline). `processes` accepts ids or names — a name resolves within project_id (default: the actor's project) first, and one that matches in several projects with no scope is a 400 naming the candidates. Idle = child_alive and now - last_output_at >= idle_ms (default idle_threshold_ms, 120000) off the BYTE STREAM, never render diffs. Processes already idle at schedule time are IGNORED until they become busy again (`any` waits for a NEW transition) and reported as already_idle. A met condition enters `confirming` and is re-checked after confirm_ms; bytes in that window re-arm it (rearm defaults true). Answers {timer_id, status: scheduled, already_idle, waiting_on, note, deadline_at}. |
| `POST` | `/timers/idle_all` | `processes`, `max_wait_ms`, `body`, `delivery_process_id?`, `idle_ms?`, `confirm_ms?`, `rearm?`, `name?`, `project_id?` | timer_fire_when_idle_all: same, but every watched process must be idle. Already-idle processes COUNT AS SATISFIED; when all of them are, the answer is {status: "already_satisfied", timer_id: null} and NO timer is created. |
| `GET` | `/timers` | `include_fired?`, `all?`, `limit?`, `offset?`, `project_id?` | timer_list: {timers: [{id, name, kind, status: pending \| confirming \| paused \| fired \| cancelled \| expired, delivery: {firing_id, process_id, status: in_flight \| delivered \| failed \| coalesced, delivered: true \| false \| null, receipt, reason: condition \| deadline \| missed \| expired, error, coalesced_with, at}, fired_count, next_fire_at, deadline_at, waiting_on, …}], total, limit, offset}. Pending first, newest first. Pending only by default; include_fired=true adds fired/cancelled/expired within timer_retention_hours. Owner-scoped (the actor that created them); all=true lists every actor's, for the orchestrator seat. project_id defaults to the actor's project. |
| `POST` | `/timers/{rid}/cancel` | — | timer_cancel: stop a pending/confirming timer (owner-scoped; the `user` actor may act on any timer). Answers {timer_id, project_id, cancelled, status}. |
| `POST` | `/timers/{rid}/pause` | — | timer_pause: STOPS THE CLOCK — the remaining delay (or remaining deadline) is captured, so a paused timer never comes due while paused. |
| `POST` | `/timers/{rid}/resume` | — | timer_resume: continue from the captured remaining delay. |

## Receipts

`/input` and `/bytes` answer `{written, bytes_len, seq_before, seq_after, had_output_within_ms, wait_ms}`.
`seq_*` is the actor's pty byte counter (byte flow, not render) read before the write and
after watching it for `wait_ms` (default 250, max 5000): `had_output_within_ms=true` means the
write produced terminal activity. Every delivery is appended to the per-process input journal.

## Liveness fields

`output_bytes` / `last_output_at` (unix ms) come from the pty reader, so they move even while
the parser is busy. `has_output=false` = still booting (distinguishable from dead);
`child_alive=false` = the child's exit has been observed. `seq` is the frame sequence the
debug listing also reports. Process stats (cpu/mem/subprocess counts) are not in
these rows; they arrive on the `term://stats` channel.

## Agents

Process rows carry `kind` (`shell` | `agent`) and, for agents, an `agent` block:
`{tool_id, tool_type, model, runtime, project_id, spawned_at_ms, spawn_uuid, nspid (pid-file | unresolved),
container? {status: running | exited | missing | unknown, started_at, uptime_s}, bridge}`.
`bridge` is `ok` | `container-down` (client alive, container not running or restarted since the
spawn) | `stale` (container running, no output for `agent_stale_after_s`, and a winsize poke
produced nothing within 2 s). The status enum itself is untouched. The probe
runs every 5 s while a docker-exec agent is alive and emits `agent://bridge {id, bridge, container}`
once per transition. The close verification (`gone` | `still-present` | `container-down` |
`unresolved`) is answered by the close route, not stored on the row.

A queued `prompt` is delivered after the first output plus `agent_ready_quiet_ms` of silence, or —
for a TUI that never goes quiet — `agent_ready_max_wait_ms` (default 5000) after the first byte
(`prompt_receipt.reason = ready-by-timeout`). Busy counting treats an agent spawned within the
last 120 s as busy even before its first byte.

Docker-exec spawns run the program as
`sh -c 'mkdir -p /tmp/.chappa-ai && echo $$ > /tmp/.chappa-ai/<uuid>.pid && exec "$0" "$@"' program args…`
so the NAMESPACE pid is known; close kills that pid (never a `docker top` pid — those are host
pids), verifies with `test -d /proc/<pid>`, then kills the host client, then removes the pid file.
Without `sh` in the container the spawn runs unwrapped (`nspid: unresolved`) and close matches
`CHAPPA_AI_SPAWN_UUID=<uuid>` in `/proc/*/environ` instead.

**Json transport.** A tool with `transport: json` runs its CLI in machine mode over
PIPES (no pty, no VT parsing; docker runtimes use `-i` without `-t`): claude
`-p --output-format stream-json --input-format stream-json --verbose` (one persistent child,
user messages as stream-json lines on stdin), opencode `run --format json` (one child per turn,
later messages continue the session with `-s <sessionID>`). Machine-mode flags go BEFORE the
tool's own args. Each stdout line maps through a per-CLI adapter to a typed event pushed as
`agent://event {id, seq, ts, kind, payload}` and kept in a 1000-event ring (`/agent_events`).
The agent block gains `transport` and `awaiting_input` — derived at read time from the json
agent, never mirrored: true after claude's `result` or an opencode child's exit, false again
on the next send, never on a dead agent (a persistent child's exit, or a per-turn
continuation that cannot spawn, retires the entry) — the rail's `agent-waiting` attention.
Close order for a docker json agent is the same as for a pty agent: container side first,
then the host `docker exec -i` client, then the pid file.
`prompt` on a json spawn is the first message; its `prompt_receipt` is the turn receipt
(`seq_before`/`seq_after` are event-ring cursors there, `had_output_within_ms` = delivered).

## Coordination

Scratchpads and todos live in `<app-config>/coordination.db` (SQLite, WAL, `PRAGMA user_version`
migrations), the app's own file, shared with nothing else. Every write is attributed to the `X-Chappa-Actor`
request header (chappa-ai-mcp sends its own `CHAPPA_AI_PROCESS_ID`; absent = `user`) as
`updated_by`. Scratchpads are scoped to a chappa-ai project id or global (`project_id: null`);
todos always belong to one project.

Typed errors (JSON `error` word, HTTP status): `revision_conflict` (409, carries
`current_revision`; on the content-bearing scratchpad ops write/edit/clear it also carries
`current_content`, capped at 8 KB with `current_content_truncated`), `locked` (409,
`locked_by`, `lock_expires_at`), `cycle` (409), `not_found` (404), `invalid` (400).
`expected_revision` is REQUIRED on write/edit/rename/clear/delete/tags and optional on
append/append_section. Archive does not bump the revision. The leading-H1 title override
applies on create and full overwrite only — edit never renames.
Write routes answer slim receipts (`{id, project_id, revision}` + the changed fields);
`response_mode: "rich"` answers the full row. Timestamps are unix ms.

Trust model: `X-Chappa-Actor` is UNAUTHENTICATED attribution — any bearer of the control
token can claim any actor id; the token is the only credential. Todo locks are therefore
advisory coordination between cooperating agents (a courtesy that prevents lost updates),
not access control; nothing here isolates one agent from another.

## Timers

Timers wake an agent with a body injected VERBATIM as a fresh user turn, through the agent delivery path (ready gate → ONE atomic write of text + `\r` → receipt) under the QUIET-ONLY policy: a timer body waits for genuine quiet up to `timer_delivery_timeout_ms` and is never typed into an agent mid-turn on the spawn prompt's ready-by-timeout fallback. They persist in `coordination.db` (`timers` / `timer_firings`, schema version 3), so an app restart does not lose a pending wake-up.

**Targets are pinned by process UUID.** Registry ids restart at 1 every launch, so a timer stores `(id, uuid)` for its delivery target (which must be LIVE when the timer is set) and for every watched process. A target whose uuid no longer matches is GONE: a delay timer (repeating or not) whose target is gone at fire time `expired`s with `{delivered: false, error: "process gone"}`, and nothing is ever typed into a shell that reused the number.

**Idle comes from the BYTE STREAM only**: a process is idle when `child_alive` and `now - last_output_at >= idle_ms` (default `idle_threshold_ms`, 120 000 — the "trust idle only past 120 s" rule made server-side). Never from render diffs; a process that has produced no output at all is booting, not idle.

**Fire-time re-validation**: a met idle condition does NOT fire. The timer enters `confirming` and is re-checked after `confirm_ms` (default 5 000); bytes in that window put it back to `pending` and, with `rearm: true` (the default), it keeps waiting. `max_wait_ms` is a hard deadline that fires the body with `reason: "deadline"` regardless.

**Every firing is auditable**: `timer_list(include_fired: true)` returns fired timers for `timer_retention_hours` (default 24) with `delivery: {firing_id, process_id, status: in_flight | delivered | failed | coalesced, delivered: true | false | null, receipt, reason, error, coalesced_with, at}`. The audit row is written IN FLIGHT before the body is queued and finished with the receipt; a body that could not be delivered is `delivered: false` with the reason in `error` (`process gone`, `not ready within …`, `interrupted by a restart …`) — never silently dropped. The list is pending first, newest first, paged by `limit` + `offset` with the unpaged `total`.

**Duplicate suppression**: an identical body to the same process within `timer_dedupe_ms` (default 5 000) — delivered, OR still in flight — is coalesced: `status: "coalesced", delivered: null, coalesced_with: <firing id>` instead of re-typed. A repeating timer never queues a second body while its previous firing is still in flight, and never coalesces against its own COMPLETED firing.

On startup, PENDING absolute-time timers whose fire time passed while the app was down are advanced ONCE and a `reason: "missed"` firing is recorded in flight (a repeating one then continues on a fresh interval — never a burst of catch-up firings). It is delivered only if the SAME spawn (by uuid) is live within `timer_missed_grace_ms` (120 000); otherwise the row closes as `{delivered: false, error: "process gone", reason: "missed"}` without typing anywhere — after a relaunch that is the normal outcome, and the audit row is what an orchestrator sees. Idle timers resume watching. An idle timer whose every watched process is gone stops with `status: "expired"` and an audit row `{delivered: false, error: "process gone"}` rather than claiming a process went quiet.
