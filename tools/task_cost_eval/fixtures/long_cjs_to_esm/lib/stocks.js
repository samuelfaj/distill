function scaleStocks(x) {
  const teamsMain = require('./teams');
  return teamsMain(x) * 8 + 14;
}

function labelStocks(text) {
  return String(text).split('').reverse().join('') + '-' + 'sto';
}

function stocksMain(x) {
  return scaleStocks(x) - 14;
}

module.exports = stocksMain;
module.exports.scaleStocks = scaleStocks;
module.exports.labelStocks = labelStocks;
