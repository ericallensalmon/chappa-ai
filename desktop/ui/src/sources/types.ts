// Wire types of the source contract (version 1). A source is a local
// process that serves these shapes over loopback HTTP; the app fetches
// them from Rust and hands them to the views. Unknown fields are ignored,
// and changes within version 1 are additive only.

/** ISO 8601 timestamp in UTC, ending in Z. */
export type Utc = string;

/** A link the user can open. Its host must be one the user trusted for the source. */
export interface Link {
  label: string;
  url: string;
}

/** A small coloured chip next to a row. */
export interface Badge {
  text: string;
  tone?: "info" | "ok" | "warn" | "bad" | "muted";
  /** Tooltip. */
  title?: string;
}

export interface Person {
  /** The source's own id for the person. */
  id?: string;
  name: string;
  /** True when this person is the signed-in user. */
  isMe?: boolean;
}

/** How current a list is, and what went wrong while building it. */
export interface Freshness {
  /** When the data was gathered; null when it never was. */
  asOf: Utc | null;
  /** The source is refreshing right now; what is served may be stale. */
  refreshing?: boolean;
  /** When the source expects to refresh next. */
  nextUtc?: Utc | null;
  /** Partial failure: render what came back and list what did not. */
  errors?: { source: string; message: string }[];
  /**
   * Informational lines about what was served, not failures: "showing 50 of
   * 212" when a search hit its cap. Shown quietly, never as a warning.
   */
  notices?: string[];
}

/** Body of every non-2xx reply. */
export interface SourceError {
  kind: string;
  message: string;
  detail?: string;
}

// ---- Work ----

export interface WorkItem {
  /** Stable and unique within the source (ABC-123, owner/repo#12). */
  key: string;
  title: string | null;
  type?: string | null;
  status: { label: string; category?: "todo" | "doing" | "done" | null } | null;
  url?: string | null;
  assignedToMe?: boolean;
  priority?: string | null;
  labels?: string[];
  parent?: { key: string; title?: string | null } | null;
  created?: Utc | null;
  reporter?: Person | null;
  lastTouchedUtc?: Utc | null;
  /** Working directory for "agent here" / "terminal here". Must resolve inside an allowed root. */
  folder?: string | null;
  prs?: { forge: string; repo: string; number: string; url?: string; status?: string }[];
  /** The item the environment is currently set up for. */
  active?: boolean;
  localStatus?: LocalStatus | null;
  badges?: Badge[];
  sections?: Section[];
  actions?: Action[];
  /**
   * Set: this entry ADDS TO the item `key` of the source whose manifest id
   * is `source` (a workspace source adding its folder to a tracker's item).
   * The owner's tracker fields win; this entry's folder, active, local
   * status and branches win; badges, banners, sections, actions and PRs
   * are added. Its title and url are shown only when the owner does not
   * list the item.
   */
  ref?: { source: string; key: string } | null;
  /** The item's branches, in the PRs' own terms: a PR whose source branch
   *  is one of them (on the same forge and repo) joins the item. */
  branches?: { forge: string; repo: string; branch: string }[];
  /** Shown at the top of the item's panel and as a dot on its row. */
  banners?: Banner[];
}

export interface WorkListResponse {
  freshness: Freshness;
  items: WorkItem[];
  banners?: Banner[];
}

/** An optional local workflow on an item. Omit it and the status block hides. */
export interface LocalStatus {
  id: string;
  label: string;
  since?: Utc | null;
  actor?: string | null;
  round?: number | null;
  /** Who may leave this status: anyone, a human, or a human at exit only. */
  gate?: "any" | "human" | "human-exit" | null;
  /** A move waiting for a human to approve it. Counts as needing the user. */
  pendingApproval?: { to: string; by?: string; note?: string } | null;
  /** The moves available from here. */
  allowed: { id: string; label: string; humanOnly?: boolean; requireNote?: boolean }[];
  history?: { at: Utc; from?: string; to?: string; actor?: string; note?: string; override?: string }[];
}

// ---- PRs ----

/**
 * Which group a PR sits in. For role "mine": action, draft, ready, waiting,
 * recent. For role "reviewing": replied (the author answered my thread),
 * vote (my vote is asked for and not given), mythreads (I commented, nothing
 * new), voted (I voted, nothing new), group (I am on it only through a team).
 *
 * The bucket says where the ball is; the app decides what needs the user
 * from it: my PRs in action / draft / ready, and reviews in replied, or in
 * vote while `me.required` is set. An optional or team-only review never
 * raises attention, whatever its bucket.
 */
export type Bucket =
  | "action" | "draft" | "ready" | "waiting" | "recent"
  | "replied" | "vote" | "mythreads" | "voted" | "group";

/**
 * One pull (or merge) request. The same PR served by two sources is one row:
 * rows are keyed by `(forge, repo, number)`, the payload with the fresher
 * `freshness.asOf` supplies the fields, and the actions of every source that
 * served it are offered, each run against its own source.
 */
export interface Pr {
  /** "github", "gitlab", "azure-devops", ... */
  forge: string;
  repo: string;
  /** A grouping above the repo on forges that have one; absent or null elsewhere. */
  project?: string | null;
  /** As shown to the user (#12, !345). */
  number: string;
  url: string;
  title: string;
  author: Person;
  sourceBranch?: string;
  targetBranch?: string;
  state: "open" | "draft" | "merged" | "closed";
  closedUtc?: Utc | null;
  role: "mine" | "reviewing";
  /** Computed by the source. */
  bucket: Bucket;
  /** Why the PR is in its bucket; shown verbatim, one line each. */
  reasons: string[];
  /** The signed-in user as a reviewer (role "reviewing"). */
  me?: {
    /**
     * The review is required of me by name. Each source decides what that
     * means on its forge: on one without a per-reviewer required flag
     * (GitHub) it is "asked for by name, not only through a group". A
     * required review is listed under Reviewing, and its `vote` bucket
     * needs the user; one that is not required (optional, or team-only with
     * bucket `group`) is listed under All only, and never needs the user.
     */
    required?: boolean;
    /**
     * My vote. "waiting" means I voted "waiting for the author", never "has
     * not voted" (that is "none"). "approved-with-suggestions" and "waiting"
     * exist for forges with a vote scale and may never appear from others.
     */
    vote?: "approved" | "approved-with-suggestions" | "waiting" | "rejected" | "none";
  } | null;
  /** Other reviewers; `required` and `vote` read as on `me`. An entry with
   *  `isGroup` is a team: a PR that reaches me only through a team (bucket
   *  `group`) is listed under its first team's name, the others as chips. */
  reviewers?: (Person & { required?: boolean; isGroup?: boolean; vote?: string })[];
  checks?: {
    state: "passing" | "failing" | "pending" | "none";
    failing?: string[];
    pending?: string[];
  } | null;
  threads?: { active: number; resolved: number; needsMe: number } | null;
  mergeable?: "yes" | "conflicts" | "unknown" | null;
  /** Keys of the Work items this PR belongs to. */
  workKeys?: string[];
  badges?: Badge[];
  /** Extra facts a forge has (check runs, deployments), shown folded in
   *  the row. Their actions belong to the PR. */
  sections?: Section[];
  actions?: Action[];
}

export interface PrListResponse {
  freshness: Freshness;
  items: Pr[];
  banners?: Banner[];
}

// ---- Needs you ----

/** Attention the source raises beyond what the app derives itself. */
export interface AttentionItem {
  /** Stable; notifications fire on ids not seen before. */
  id: string;
  title: string;
  detail: string;
  /** Section heading the item is listed under. */
  group: string;
  severity?: "info" | "warn" | "bad";
  opens?: { view: "work" | "prs" | "environment"; key: string } | { url: string };
  actions?: Action[];
}

export interface AttentionListResponse {
  freshness: Freshness;
  items: AttentionItem[];
}

// ---- Environment ----

export type ComponentState =
  | "up" | "starting" | "unhealthy"
  | "crashloop" | "stopped-problem" | "stopped-idle"
  | "oneshot-ok" | "oneshot-failed"
  | "not-deployed";

export interface Component {
  id: string;
  /** Id of an entry in EnvironmentResponse.groups. */
  group: string;
  label?: string;
  /** A oneshot runs to completion (migrations, seed jobs). Default "service". */
  kind?: "service" | "oneshot";
  state: ComponentState;
  uptime?: string | null;
  restarts?: number | null;
  exitCode?: number | null;
  source?: { mode: "prebuilt" | "source" | "dev"; ref?: string | null; commit?: string | null } | null;
  links?: (Link & { browser?: boolean })[];
  /**
   * An HTTP check. "health" passes on 2xx only, "web" on 2xx or 3xx, "any"
   * on any status; no response at all is always down.
   */
  probe?: { url: string; rule: "health" | "web" | "any"; up: boolean; status?: number | null } | null;
  hint?: string | null;
  badges?: Badge[];
  /** Extra facts (configuration, volumes), shown folded in the row. Their
   *  actions belong to the component. */
  sections?: Section[];
  actions?: Action[];
}

export interface EnvironmentResponse {
  freshness: Freshness;
  /** Count per state; the header shows the non-zero ones. */
  summary?: Partial<Record<ComponentState, number>>;
  /** Display order. A group's `actions` are drawn on its header; they are
   *  judged and called with `target.key` = the group's id. */
  groups: { id: string; label: string; actions?: Action[] }[];
  components: Component[];
  /** Stack-wide actions, drawn once in the view's header; judged and
   *  called with `target.key` = null, like the view's banners'. */
  actions?: Action[];
  /** Runtimes, memory, dirty clones and the like. A section with
   *  `kind: "meta"` describes the runner instead (shown apart, muted); one
   *  with a `status` is drawn as that one badge, its rows the details. */
  facts?: Section[];
  /** Drives the view's alert dot. `kind: "state"`: the alert only restates
   *  the component's red state, which the view already lists in its problem
   *  banner, so it is not drawn again as a line. */
  alerts?: { text: string; severity: "warn" | "bad"; componentId?: string; kind?: "state" }[];
  banners?: Banner[];
}

// ---- Actions, sections, banners, panels ----

/** Extra environment for a launched agent or terminal; names match [A-Z_][A-Z0-9_]*. */
export type ActionEnv = Record<string, string>;

export type Action =
  | {
      kind: "agent"; id: string; label: string;
      cwd: string;
      /** First prompt; shown to the user before it runs. */
      prompt: string;
      /** Agent tool name; absent means the project's default agent tool. */
      tool?: string;
      title?: string;
      env?: ActionEnv;
    }
  | {
      kind: "terminal"; id: string; label: string;
      cwd: string;
      /** argv, never a shell string. */
      command?: string[];
      env?: ActionEnv;
    }
  | {
      kind: "endpoint"; id: string; label: string;
      /** True when the call changes something. */
      mutates: boolean;
      /** Question shown before the call. Required when mutates: without it the action is not rendered. */
      confirm?: string;
      params?: Record<string, unknown>;
      /** Whether the user may or must add a note. */
      note?: "optional" | "required";
      /** What the confirm dialog asks for; each value goes into `params`
       *  under its name (replacing a fixed param of that name), checked by
       *  the app against the declaration before the call. */
      inputs?: EndpointInput[];
      /** Set: offered but not accepted now; drawn disabled with this as its
       *  tooltip, and the app refuses a call. Trimmed, at most 200 chars. */
      disabled?: string;
    }
  | { kind: "link"; id: string; label: string; url: string }
  /** Opens a drill-in page served by the source's endpoint. */
  | { kind: "panel"; id: string; label: string; endpoint: string };

export type SectionTone = "ok" | "warn" | "bad" | "muted";

/** A titled block of label/value rows. */
export interface Section {
  id: string;
  title: string;
  collapsed?: boolean;
  /** Where the Environment header shows it: "meta" = about the runner
   *  (instance, slot, bound item), not the stack's health. */
  kind?: "meta";
  /** Set: the Environment header shows the section as one badge
   *  ("Data: clean ✓") opening its rows. */
  status?: { label: string; text: string; tone: SectionTone };
  /** `tone` colours the row's chip in the Environment facts line. */
  rows: { label: string; value?: string; links?: Link[]; badges?: Badge[]; tone?: SectionTone }[];
  /** Rows folded behind a toggle labelled `label`. */
  more?: { label: string; rows: Section["rows"] };
  actions?: Action[];
}

export interface Banner {
  id: string;
  tone: "info" | "warn" | "bad";
  title: string;
  lines?: string[];
  actions?: Action[];
}

/** Body of an endpoint action call. */
export interface ActionRequest {
  id: string;
  /**
   * `source` and `item` are set on calls about a row another source owns:
   * an item action, or an action of an entry that adds to another source's
   * item. `source` is the owner's manifest id, `item` the owner's row as
   * the app last received it (left out when the owner does not list it).
   * Untrusted input to the receiving source.
   */
  target: { view: string; key: string | null; source?: string; item?: WorkItem | Pr };
  params?: Record<string, unknown>;
  note?: string;
  /** Set only after the user accepted the action's confirm text. A mutating call without it is refused. */
  confirmed?: boolean;
}

/** The job the app runs for a 202 reply that carries one. */
export interface RunBlock {
  argv: string[];
  cwd: string;
  env?: ActionEnv;
  title?: string;
}

/** Reply to an endpoint action: 200 done, 202 started (optionally with a run), 503 outcome unknown. */
export interface ActionResult {
  message?: string;
  jobId?: string;
  run?: RunBlock;
}

export interface JobStatus {
  state: "running" | "done" | "failed";
  log?: string;
}

/** What the app posts to the jobs endpoint when a `run` job's pane exits. */
export interface JobExitReport {
  exitCode: number;
  endedUtc: Utc;
  /** The pane's text, capped with a truncation marker. */
  output: string;
}

/** A drill-in page. The app re-polls it while it is open. */
export interface PanelResponse {
  title: string;
  freshness: Freshness;
  banners?: Banner[];
  sections: Section[];
  actions?: Action[];
}

// ---- Manifest ----

export interface ViewEndpoint {
  list: string;
  /** Item detail path with a {key} placeholder (work only). */
  item?: string;
  /** Clamped by the app to 15..3600. */
  pollSeconds?: number;
}

/** `GET /chappa/v1/manifest`. Every view is optional; the app calls only the paths named here. */
export interface Manifest {
  contract: 1;
  source: { id: string; label: string; version: string };
  views: {
    work?: ViewEndpoint;
    prs?: ViewEndpoint;
    attention?: ViewEndpoint;
    environment?: ViewEndpoint;
  };
  /** Optional list of the source's local workflows. */
  workflows?: string;
  /** POST target for endpoint actions. */
  actions?: string;
  /** Job status path with an {id} placeholder. */
  jobs?: string;
  /** Hosts links may open on; the user approves this list. `host:*` is
   *  any port on that host. */
  linkHosts: string[];
  /** Extra directories the source wants to launch in; the user approves them. */
  roots?: string[];
  /** Where the app posts `{source, key, item}` each time a row of another
   *  source that this one adds to changes (after that source's Work poll).
   *  Part of what the user trusts. */
  ownerUpdates?: string;
  /** Actions offered on OTHER sources' rows (endpoint only; a mutating one
   *  needs `confirm`, where `{key}`, `{title}` and `{source}` are filled by
   *  the app). Part of what the user trusts. */
  itemActions?: (Extract<Action, { kind: "endpoint" }> & { on: { view: "work" | "prs"; sources: string[] | "*" } })[];
}

/** One input of an endpoint action: a choice, several choices, one line of
 *  text (at most `max` characters, default 200) or a number (at most `max`). */
export interface EndpointInput {
  name: string;
  label: string;
  kind: "choice" | "multi" | "text" | "number";
  options?: { value: string; label: string }[];
  required?: boolean;
  default?: unknown;
  max?: number;
}
