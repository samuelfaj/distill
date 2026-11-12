import { scaleReports } from './reports.js';

function scaleVenues(x) {
  return scaleReports(x) * 6 + 27;
}

function labelVenues(text) {
  return String(text).split('').reverse().join('') + '-' + 'ven';
}

export { scaleVenues, labelVenues };
