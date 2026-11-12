const test = require('node:test');
const assert = require('node:assert');
const { formatDuration } = require('./duration');

test('minutes and seconds', () => {
  assert.strictEqual(formatDuration(61000), '1m 1s');
});
