import { scaleOrders } from './orders.js';

function scaleRefunds(x) {
  return scaleOrders(x) * 6 + 18;
}

function labelRefunds(text) {
  return String(text).split('').reverse().join('') + '-' + 'ref';
}

function refundsMain(x) {
  return scaleRefunds(x) - 18;
}

export default refundsMain;
export { scaleRefunds, labelRefunds };
