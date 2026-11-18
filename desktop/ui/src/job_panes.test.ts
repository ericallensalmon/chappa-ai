import { describe, expect, it } from "vitest";
import type * as ipc from "./ipc";
import {
  CLOSE_DELAY_MS,
  JobPaneCloser,
  cancelQuestion,
  doneText,
  quitQuestion,
  runningJobInPane,
} from "./job_panes";

function job(over: Partial<ipc.SourceJobDto> = {}): ipc.SourceJobDto {
  return {
    jobId: "j1",
    label: "stack: start",
    actionId: "start",
    view: "environment",
    key: null,
    startedUtc: "2026-11-14T12:00:00Z",
    state: "running",
    run: true,
    termId: 7,
    exitCode: null,
    endedUtc: null,
    log: null,
    ...over,
  };
}

function source(jobs: ipc.SourceDto["jobs"]): ipc.SourceDto {
  return { id: "s1", label: "Stack", jobs } as unknown as ipc.SourceDto;
}

function harness(opts: { enabled?: boolean; closable?: (id: number) => boolean } = {}) {
  const timers: { fn: () => void; ms: number }[] = [];
  const closed: number[] = [];
  const said: string[] = [];
  let enabled = opts.enabled ?? true;
  const closer = new JobPaneCloser({
    enabled: () => enabled,
    closable: opts.closable ?? (() => true),
    close: (id) => closed.push(id),
    say: (t) => said.push(t),
    setTimer: (fn, ms) => timers.push({ fn, ms }),
  });
  const fire = () => timers.splice(0).forEach((t) => t.fn());
  return { closer, timers, closed, said, fire, setEnabled: (v: boolean) => (enabled = v) };
}

describe("job panes", () => {
  it("closes a done job's pane after the delay and says so", () => {
    const h = harness();
    h.closer.observe([source([job()])]);
    h.closer.observe([source([job({ state: "done", exitCode: 0 })])]);
    expect(h.said).toEqual([doneText(job())]);
    expect(h.said[0]).toContain("Last job's log");
    expect(h.timers.map((t) => t.ms)).toEqual([CLOSE_DELAY_MS]);
    expect(h.closed).toEqual([]);
    h.fire();
    expect(h.closed).toEqual([7]);
  });

  it("keeps the pane of a job that failed, and acts once per job", () => {
    const h = harness();
    h.closer.observe([source([job()])]);
    h.closer.observe([source([job({ state: "failed", exitCode: 1 })])]);
    h.closer.observe([source([job({ state: "failed", exitCode: 1 })])]);
    expect(h.timers).toEqual([]);
    expect(h.said).toEqual([]);
  });

  it("leaves a pane the person typed or scrolled in, or is looking at", () => {
    const h = harness({ closable: () => false });
    h.closer.observe([source([job()])]);
    h.closer.observe([source([job({ state: "done" })])]);
    h.fire();
    expect(h.closed).toEqual([]);
    expect(h.said.length).toBe(1);
  });

  it("closes nothing with the setting off (also when turned off during the delay)", () => {
    const off = harness({ enabled: false });
    off.closer.observe([source([job()])]);
    off.closer.observe([source([job({ state: "done" })])]);
    expect(off.timers).toEqual([]);
    expect(off.said.length).toBe(1);
    const late = harness();
    late.closer.observe([source([job()])]);
    late.closer.observe([source([job({ state: "done" })])]);
    late.setEnabled(false);
    late.fire();
    expect(late.closed).toEqual([]);
  });

  it("does not act on jobs that had ended before the first look", () => {
    const h = harness();
    h.closer.observe([source([job({ state: "done" })])]);
    expect(h.said).toEqual([]);
    // A job first seen already done after that is acted on.
    h.closer.observe([source([job({ state: "done" }), job({ jobId: "j2", termId: 8, state: "done" })])]);
    h.fire();
    expect(h.closed).toEqual([8]);
  });

  it("finds the running job in a pane and words the questions", () => {
    const sources = [source([job({ state: "done", termId: 3 }), job({ jobId: "j2", termId: 4 })])];
    expect(runningJobInPane(sources, 3)).toBeNull();
    expect(runningJobInPane(sources, 4)?.job.jobId).toBe("j2");
    expect(cancelQuestion(job())).toContain('"stack: start" is still running');
    expect(quitQuestion([{ label: "a" }])).toContain("A job is running in a pane");
    expect(quitQuestion([{ label: "a" }, { label: "b" }])).toContain("2 jobs are running in panes");
  });
});
