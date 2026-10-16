// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
// Native commands can complete out of order; never publish pre-control snapshots afterward.
export class RequestGate {
  private epoch = 0;
  invalidate(): void {
    this.epoch += 1;
  }
  async receive<T>(
    load: () => Promise<T>,
    publish: (value: T) => void,
  ): Promise<void> {
    const epoch = this.epoch;
    const value = await load();
    if (epoch === this.epoch) publish(value);
  }
  async attempt<T>(
    load: () => Promise<T>,
    publish: (value: T) => void,
    fail: (error: unknown) => void,
  ): Promise<void> {
    const epoch = this.epoch;
    try {
      const value = await load();
      if (epoch === this.epoch) publish(value);
    } catch (error) {
      if (epoch === this.epoch) fail(error);
    }
  }
}
