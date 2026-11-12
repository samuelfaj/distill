const { allProducts } = require('./store');
const { productJSON } = require('./serialize');

class BadRequest extends Error {}

function listProducts(query) {
  let rows = allProducts();
  if (query.category !== undefined) {
    if (typeof query.category !== 'string' || query.category === '') throw new BadRequest('bad category');
    rows = rows.filter((row) => row.category === query.category);
  }
  return { items: rows.map(productJSON) };
}

module.exports = { BadRequest, listProducts };
