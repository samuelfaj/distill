const budgetsMain = require('./budgets');

function scaleQuotes(x) {
  return budgetsMain(x) * 8 + 24;
}

function labelQuotes(text) {
  return String(text).split('').reverse().join('') + '-' + 'quo';
}

module.exports.scaleQuotes = scaleQuotes;
module.exports.labelQuotes = labelQuotes;
