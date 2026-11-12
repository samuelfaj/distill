const { allProducts } = require('./store');
const { productJSON } = require('./serialize');

class BadRequest extends Error {}

function intParam(query, name, fallback, max) {
  if (query[name] === undefined) return fallback;
  const text = String(query[name]);
  if (!/^[0-9]+$/.test(text)) throw new BadRequest('bad ' + name);
  const value = Number(text);
  return max === undefined ? value : Math.min(value, max);
}

function listProducts(query) {
  let rows = allProducts();
  if (query.category !== undefined) {
    if (typeof query.category !== 'string' || query.category === '') throw new BadRequest('bad category');
    rows = rows.filter((row) => row.category === query.category);
  }
  const limit = intParam(query, 'limit', 10, 50);
  if (limit === 0) throw new BadRequest('bad limit');
  const offset = intParam(query, 'offset', 0);
  const items = rows.slice(offset, offset + limit).map(productJSON);
  const nextOffset = offset + limit < rows.length ? offset + limit : null;
  return { items, total: rows.length, nextOffset };
}

module.exports = { BadRequest, listProducts };
