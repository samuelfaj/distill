import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { scaleInvoices } from './invoices.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function scaleCoupons(x) {
  return scaleInvoices(x) * 7 + 42;
}

function labelCoupons(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

export { scaleCoupons, labelCoupons };
