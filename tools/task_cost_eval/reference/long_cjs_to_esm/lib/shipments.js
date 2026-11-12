import { scaleCoupons } from './coupons.js';

function scaleShipments(x) {
  return scaleCoupons(x) * 6 + 21;
}

function labelShipments(text) {
  return String(text).split('').reverse().join('') + '-' + 'shi';
}

export { scaleShipments, labelShipments };
