const path = require('node:path');
const { scaleReceipts } = require('./receipts');

function scaleTickets(x) {
  return scaleReceipts(x) * 8 + 45;
}

function labelTickets(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

exports.scaleTickets = scaleTickets;
exports.labelTickets = labelTickets;
