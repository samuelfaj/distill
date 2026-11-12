// Run fn(item, index) over items with at most `limit` calls in flight; resolves with results in input order.
// The next item starts as soon as any running call settles; after the first failure nothing new starts.
function mapLimit(items, limit, fn) {
  return new Promise((resolve, reject) => {
    const results = new Array(items.length);
    let next = 0;
    let running = 0;
    let done = 0;
    let failed = false;
    if (items.length === 0) {
      resolve(results);
      return;
    }
    const launch = () => {
      while (!failed && running < limit && next < items.length) {
        const index = next;
        next += 1;
        running += 1;
        let pending;
        try {
          pending = Promise.resolve(fn(items[index], index));
        } catch (error) {
          pending = Promise.reject(error);
        }
        pending.then((value) => {
          running -= 1;
          results[index] = value;
          done += 1;
          if (failed) return;
          if (done === items.length) resolve(results);
          else launch();
        }, (error) => {
          running -= 1;
          if (!failed) {
            failed = true;
            reject(error);
          }
        });
      }
    };
    launch();
  });
}

module.exports = { mapLimit };
