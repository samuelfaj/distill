const stocksMain = require('./stocks');

function scaleBatches(x) {
  return stocksMain(x) * 5 + 13;
}

function labelBatches(text) {
  return String(text).split('').reverse().join('') + '-' + 'bat';
}

module.exports.scaleBatches = scaleBatches;
module.exports.labelBatches = labelBatches;
