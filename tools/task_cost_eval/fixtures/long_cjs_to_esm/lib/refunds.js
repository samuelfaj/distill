const { scaleOrders } = require('./orders');

function scaleRefunds(x) {
  return scaleOrders(x) * 6 + 18;
}

function labelRefunds(text) {
  return String(text).split('').reverse().join('') + '-' + 'ref';
}

function refundsMain(x) {
  return scaleRefunds(x) - 18;
}

module.exports = refundsMain;
module.exports.scaleRefunds = scaleRefunds;
module.exports.labelRefunds = labelRefunds;
