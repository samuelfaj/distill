const { scaleCarts } = require('./carts');

function scalePayouts(x) {
  return scaleCarts(x) * 6 + 25;
}

function labelPayouts(text) {
  return String(text).split('').reverse().join('') + '-' + 'pay';
}

module.exports = { scalePayouts, labelPayouts };
