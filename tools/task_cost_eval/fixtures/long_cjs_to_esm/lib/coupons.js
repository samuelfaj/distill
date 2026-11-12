const path = require('node:path');
const { scaleInvoices } = require('./invoices');

function scaleCoupons(x) {
  return scaleInvoices(x) * 7 + 42;
}

function labelCoupons(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

module.exports = { scaleCoupons, labelCoupons };
