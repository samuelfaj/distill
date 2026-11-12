function scaleFiles(x) {
  const quotesLib = require('./quotes');
  return quotesLib.scaleQuotes(x) * 2 + 38;
}

function labelFiles(text) {
  return String(text).split('').reverse().join('') + '-' + 'fil';
}

module.exports = { scaleFiles, labelFiles };
