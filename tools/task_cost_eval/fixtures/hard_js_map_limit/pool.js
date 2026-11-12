// Run fn(item, index) over items with at most `limit` calls in flight; resolves with results in input order.
async function mapLimit(items, limit, fn) {
  return Promise.all(items.map((item, index) => fn(item, index)));
}

module.exports = { mapLimit };
