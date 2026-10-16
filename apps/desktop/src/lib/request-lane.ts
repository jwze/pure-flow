// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
import { RequestGate } from "./request-gate.ts";

type Job = () => Promise<void>;

// One in-flight native request plus at most one (latest) queued request.
// Invalidation also drops queued work on hide/unmount/tab change.
export class RequestLane {
  private gate = new RequestGate();
  private queued: Job | undefined;
  private active = false;
  private changed: (busy: boolean) => void;
  constructor(changed: (busy: boolean) => void = () => {}) { this.changed = changed; }
  get busy(): boolean { return this.active; }
  invalidate(): void {
    this.gate.invalidate();
    this.queued = undefined;
  }
  submit<T>(load: () => Promise<T>, publish: (value: T) => void, fail: (error: unknown) => void): void {
    this.gate.invalidate();
    const job = () => this.gate.attempt(load, publish, fail);
    if (this.active) this.queued = job;
    else void this.drain(job);
  }
  // Live revisions do not invalidate a slow in-flight read within the same lifecycle.
  // Explicit invalidate() still rejects pre-control/hidden/tab-switched results.
  submitLatest<T>(load: () => Promise<T>, publish: (value: T) => void, fail: (error: unknown) => void): void {
    const job = () => this.gate.attempt(load, publish, fail);
    if (this.active) this.queued = job;
    else void this.drain(job);
  }
  private async drain(job: Job): Promise<void> {
    this.active = true;
    this.changed(true);
    try {
      await job();
    } finally {
      this.active = false;
      const next = this.queued;
      this.queued = undefined;
      if (next) void this.drain(next);
      else this.changed(false);
    }
  }
}
