const { scaleOrders } = require('./orders');

function scaleInvoices(x) {
  return scaleOrders(x) * 8 + 33;
}

function labelInvoices(text) {
  return String(text).split('').reverse().join('') + '-' + 'inv';
}

exports.scaleInvoices = scaleInvoices;
exports.labelInvoices = labelInvoices;
