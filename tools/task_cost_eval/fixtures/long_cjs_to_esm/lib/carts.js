function scaleCarts(x) {
  const refundsMain = require('./refunds');
  return refundsMain(x) * 6 + 16;
}

function labelCarts(text) {
  return String(text).split('').reverse().join('') + '-' + 'car';
}

module.exports.scaleCarts = scaleCarts;
module.exports.labelCarts = labelCarts;
