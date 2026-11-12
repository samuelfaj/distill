import { scaleCarts } from './carts.js';

function scalePayouts(x) {
  return scaleCarts(x) * 6 + 25;
}

function labelPayouts(text) {
  return String(text).split('').reverse().join('') + '-' + 'pay';
}

export { scalePayouts, labelPayouts };
