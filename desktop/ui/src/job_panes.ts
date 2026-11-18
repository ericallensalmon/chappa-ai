// A source job's pane, after and while the job runs.
//
// - Done: a toast names the job, and its pane closes after a few seconds
//   (Settings ▸ Terminal "Close successful job panes", on by default). The
//   result stays reachable: the job record and its log, and the stack's
//   last-job line in the Environment view. A pane the person typed or
//   scrolled in, or is looking at, stays (marked done like any exited pane).
// - Failed, cancelled, interrupted: the pane stays, marked by its exit code.
// - Closing a running job's pane asks to cancel the job instead: the source
//   stops its current step with everything it started, and the pane ends on
//   its own once the job did.

import type * as ipc from "./ipc";

/** How long a done job's pane stays before it closes. */
export const CLOSE_DELAY_MS = 5000;

export interface JobPaneDeps {
  /** The setting. Read when the job ends and again when the delay is up. */
  enabled(): boolean;
  /** Whether the pane `termId` still exists and may be closed now: nobody
   *  typed or scrolled in it, and it is not the pane on screen. */
  closable(termId: number): boolean;
  close(termId: number): void;
  say(text: string): void;
  setTimer(fn: () => void, ms: number): void;
}

/** A job of a source, as the listing carries it. */
export interface JobRef {
  sourceId: string;
  job: ipc.SourceJobDto;
}

/** What a job is called in a toast or a dialog. */
export function jobLabel(job: ipc.SourceJobDto): string {
  return job.label?.trim() || job.actionId;
}

/** The toast when a job ended `done`. */
export function doneText(job: ipc.SourceJobDto): string {
  return `${jobLabel(job)}: done. Its log stays in Environment ▸ Last job's log.`;
}

/** The running job whose pane is `termId`, if any. */
export function runningJobInPane(sources: readonly ipc.SourceDto[], termId: number): JobRef | null {
  for (const s of sources) {
    for (const job of s.jobs ?? []) {
      if (job.run && job.termId === termId && job.state === "running") return { sourceId: s.id, job };
    }
  }
  return null;
}

/** The question before a running job's pane is closed. */
export function cancelQuestion(job: ipc.SourceJobDto): string {
  return (
    `"${jobLabel(job)}" is still running in this pane. Closing it would stop the job part-way.\n\n` +
    "Cancel the job instead? Its current step is stopped with everything it started, the steps after " +
    "it do not run, and the pane stays open until the job has ended."
  );
}

/** The question before quitting while jobs run in panes. */
export function quitQuestion(jobs: readonly { label: string }[]): string {
  const names = jobs.map((j) => `• ${j.label}`).join("\n");
  const one = jobs.length === 1;
  return (
    `${one ? "A job is" : `${jobs.length} jobs are`} running in ${one ? "a pane" : "panes"} here:\n\n${names}\n\n` +
    `Quitting stops ${one ? "it" : "them"} part-way and may leave the stack half-changed. ` +
    `${one ? "It is" : "They are"} marked interrupted the next time the stack is looked at.`
  );
}

/** Watches the sources listing for jobs that end, and closes the panes of
 *  the ones that ended `done`. The first listing only primes it: a job
 *  that had ended before the app looked is not acted on. */
export class JobPaneCloser {
  private seen = new Map<string, string>();
  private primed = false;

  constructor(private readonly deps: JobPaneDeps) {}

  observe(sources: readonly ipc.SourceDto[]): void {
    const first = !this.primed;
    this.primed = true;
    for (const s of sources) {
      for (const job of s.jobs ?? []) {
        const key = `${s.id}\u001f${job.jobId}`;
        const before = this.seen.get(key);
        this.seen.set(key, job.state);
        if (first || before === job.state || job.state !== "done" || !job.run) continue;
        this.deps.say(doneText(job));
        const termId = job.termId;
        if (termId == null || !this.deps.enabled()) continue;
        this.deps.setTimer(() => {
          if (this.deps.enabled() && this.deps.closable(termId)) this.deps.close(termId);
        }, CLOSE_DELAY_MS);
      }
    }
  }
}
