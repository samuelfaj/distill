const test = require('node:test');
const assert = require('node:assert');
const { slugify } = require('./slug');

test('joins words with the default separator', () => {
  assert.strictEqual(slugify('Hello World'), 'hello_world');
});

test('explicit separator', () => {
  assert.strictEqual(slugify('a b', '.'), 'a.b');
});
