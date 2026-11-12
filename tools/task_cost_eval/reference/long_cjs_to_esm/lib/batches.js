import stocksMain from './stocks.js';

function scaleBatches(x) {
  return stocksMain(x) * 5 + 13;
}

function labelBatches(text) {
  return String(text).split('').reverse().join('') + '-' + 'bat';
}

export { scaleBatches, labelBatches };
