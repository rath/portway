// One recovery attempt at a time. Ignore replies from a stopped/replaced attempt.
export class Connection {
  constructor({ connect, connected, retrying, expired,
    setTimer = (callback, delay) => setTimeout(callback, delay),
    clearTimer = (timer) => clearTimeout(timer) }) {
    Object.assign(this, { connect, connected, retrying, expired, setTimer, clearTimer });
    this.active = false;
    this.timer = null;
    this.generation = 0;
    this.delay = 2000;
  }

  start() {
    this.stop();
    this.active = true;
    this.delay = 2000;
    void this.attempt();
  }

  stop() {
    this.active = false;
    this.generation++;
    if (this.timer !== null) this.clearTimer(this.timer);
    this.timer = null;
  }

  retry() {
    if (!this.active || this.timer !== null) return;
    this.generation++;
    this.retrying();
    this.timer = this.setTimer(() => {
      this.timer = null;
      void this.attempt();
    }, this.delay);
    this.delay = Math.min(this.delay * 2, 30000);
  }

  async attempt() {
    const generation = ++this.generation;
    try {
      const result = await this.connect();
      if (!this.active || generation !== this.generation) return;
      this.delay = 2000;
      this.connected(result);
    } catch (error) {
      if (!this.active || generation !== this.generation) return;
      if (error.status === 401) {
        this.stop();
        this.expired();
      } else {
        this.retry();
      }
    }
  }
}
