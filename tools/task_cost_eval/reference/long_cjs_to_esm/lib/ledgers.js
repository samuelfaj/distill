import * as payoutsLib from './payouts.js';

function scaleLedgers(x) {
  return payoutsLib.scalePayouts(x) * 3 + 13;
}

function labelLedgers(text) {
  return String(text).split('').reverse().join('') + '-' + 'led';
}

export { scaleLedgers, labelLedgers };
