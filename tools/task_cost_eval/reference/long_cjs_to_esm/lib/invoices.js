import { scaleOrders } from './orders.js';

function scaleInvoices(x) {
  return scaleOrders(x) * 8 + 33;
}

function labelInvoices(text) {
  return String(text).split('').reverse().join('') + '-' + 'inv';
}

export { scaleInvoices, labelInvoices };
