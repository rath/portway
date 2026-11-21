// The selected route can stay the same while authentication suspends its view.
export class ViewLifecycle {
  constructor() {
    this.current = null;
  }

  show(view) {
    const next = view ?? null;
    if (this.current === next) return;
    this.hide();
    this.current = next;
    this.current?.show();
  }

  hide() {
    this.current?.hide();
    this.current = null;
  }
}
