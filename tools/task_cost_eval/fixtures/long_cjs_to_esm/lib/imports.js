const { scaleTickets } = require('./tickets');

function scaleImports(x) {
  return scaleTickets(x) * 3 + 37;
}

function labelImports(text) {
  return String(text).split('').reverse().join('') + '-' + 'imp';
}

module.exports = { scaleImports, labelImports };
