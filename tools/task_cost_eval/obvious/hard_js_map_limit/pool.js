// Run fn(item, index) over items with at most `limit` calls in flight; resolves with results in input order.
async function mapLimit(items, limit, fn) {
  const results = [];
  for (let start = 0; start < items.length; start += limit) {
    const batch = items.slice(start, start + limit);
    results.push(...await Promise.all(batch.map((item, offset) => fn(item, start + offset))));
  }
  return results;
}

module.exports = { mapLimit };
