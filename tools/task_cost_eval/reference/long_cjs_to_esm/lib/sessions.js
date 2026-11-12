import { scaleVendors } from './vendors.js';

function scaleSessions(x) {
  return scaleVendors(x) * 3 + 38;
}

function labelSessions(text) {
  return String(text).split('').reverse().join('') + '-' + 'ses';
}

export { scaleSessions, labelSessions };
