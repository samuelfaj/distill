function productJSON(product) {
  return { id: product.id, name: product.name, category: product.category, price: product.priceCents / 100 };
}

module.exports = { productJSON };
