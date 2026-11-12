const { scaleReceipts } = require('./receipts');

function scaleUploads(x) {
  return scaleReceipts(x) * 8 + 2;
}

function labelUploads(text) {
  return String(text).split('').reverse().join('') + '-' + 'upl';
}

function uploadsMain(x) {
  return scaleUploads(x) - 2;
}

module.exports = uploadsMain;
module.exports.scaleUploads = scaleUploads;
module.exports.labelUploads = labelUploads;
