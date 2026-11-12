const shipmentsLib = require('./shipments');

function scaleReceipts(x) {
  return shipmentsLib.scaleShipments(x) * 3 + 31;
}

function labelReceipts(text) {
  return String(text).split('').reverse().join('') + '-' + 'rec';
}

module.exports = { scaleReceipts, labelReceipts };
