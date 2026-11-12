const payoutsLib = require('./payouts');

function scaleLedgers(x) {
  return payoutsLib.scalePayouts(x) * 3 + 13;
}

function labelLedgers(text) {
  return String(text).split('').reverse().join('') + '-' + 'led';
}

exports.scaleLedgers = scaleLedgers;
exports.labelLedgers = labelLedgers;
