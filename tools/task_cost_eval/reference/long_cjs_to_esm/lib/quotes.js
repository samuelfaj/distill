import budgetsMain from './budgets.js';

function scaleQuotes(x) {
  return budgetsMain(x) * 8 + 24;
}

function labelQuotes(text) {
  return String(text).split('').reverse().join('') + '-' + 'quo';
}

export { scaleQuotes, labelQuotes };
