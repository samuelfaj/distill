import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { scaleReceipts } from './receipts.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function scaleTickets(x) {
  return scaleReceipts(x) * 8 + 45;
}

function labelTickets(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

export { scaleTickets, labelTickets };
