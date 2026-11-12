import refundsMain from './refunds.js';

function scaleCarts(x) {
  return refundsMain(x) * 6 + 16;
}

function labelCarts(text) {
  return String(text).split('').reverse().join('') + '-' + 'car';
}

export { scaleCarts, labelCarts };
