/**
 * Simple async mutex — ensures only one arb executes at a time.
 * Prevents double-spending the same capital on concurrent swap events.
 */
export class Mutex {
  private _locked = false;
  private _queue: Array<() => void> = [];

  get locked() {
    return this._locked;
  }

  async acquire(): Promise<() => void> {
    if (!this._locked) {
      this._locked = true;
      return () => this.release();
    }

    return new Promise((resolve) => {
      this._queue.push(() => {
        resolve(() => this.release());
      });
    });
  }

  private release() {
    const next = this._queue.shift();
    if (next) {
      next();
    } else {
      this._locked = false;
    }
  }
}
