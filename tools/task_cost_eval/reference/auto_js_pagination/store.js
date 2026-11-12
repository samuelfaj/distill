const CATEGORIES = ['books', 'games', 'tools'];
const products = [];
for (let id = 1; id <= 73; id += 1) {
  products.push({ id, name: 'Product ' + id, category: CATEGORIES[id % 3], priceCents: 100 * id });
}

function allProducts() {
  return products.map((product) => ({ ...product }));
}

module.exports = { allProducts };
