const { BadRequest, listProducts } = require('./handlers');

function handle(method, path, query = {}) {
  try {
    if (method === 'GET' && path === '/products') return { status: 200, body: listProducts(query) };
    return { status: 404, body: { error: 'not found' } };
  } catch (error) {
    if (error instanceof BadRequest) return { status: 400, body: { error: error.message } };
    throw error;
  }
}

module.exports = { handle };
