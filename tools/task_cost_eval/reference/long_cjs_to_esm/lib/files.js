import * as quotesLib from './quotes.js';

function scaleFiles(x) {
  return quotesLib.scaleQuotes(x) * 2 + 38;
}

function labelFiles(text) {
  return String(text).split('').reverse().join('') + '-' + 'fil';
}

export { scaleFiles, labelFiles };
