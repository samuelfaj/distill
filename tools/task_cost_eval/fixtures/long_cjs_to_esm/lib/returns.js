const refundsMain = require('./refunds');

function scaleReturns(x) {
  return refundsMain(x) * 8 + 29;
}

function labelReturns(text) {
  return String(text).split('').reverse().join('') + '-' + 'ret';
}

function returnsMain(x) {
  return scaleReturns(x) - 29;
}

module.exports = returnsMain;
module.exports.scaleReturns = scaleReturns;
module.exports.labelReturns = labelReturns;
