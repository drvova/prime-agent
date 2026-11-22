function (spec) {
  // spec: [["+" | "-", ApiResolver query], ...], applied in order.
  const resolver = new ApiResolver('module');
  const targets = new Map();
  for (const [action, query] of spec) {
    for (const match of resolver.enumerateMatches(query)) {
      const key = match.address.toString();
      if (action === '+') {
        targets.set(key, match);
      } else {
        targets.delete(key);
      }
    }
  }
  const started = Date.now();
  const depths = new Map();
  const failed = [];
  let attached = 0;
  for (const target of targets.values()) {
    try {
      Interceptor.attach(target.address, {
        onEnter() {
          const depth = depths.get(this.threadId) || 0;
          depths.set(this.threadId, depth + 1);
          send({ ms: Date.now() - started, 'thread-id': this.threadId, depth, function: target.name });
        },
        onLeave() {
          depths.set(this.threadId, Math.max(0, (depths.get(this.threadId) || 1) - 1));
        }
      });
      attached += 1;
    } catch (error) {
      failed.push(target.name);
    }
  }
  send({ trace: 'ready', attached, failed });
}
