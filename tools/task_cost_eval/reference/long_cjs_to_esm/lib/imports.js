import { scaleTickets } from './tickets.js';

function scaleImports(x) {
  return scaleTickets(x) * 3 + 37;
}

function labelImports(text) {
  return String(text).split('').reverse().join('') + '-' + 'imp';
}

export { scaleImports, labelImports };
