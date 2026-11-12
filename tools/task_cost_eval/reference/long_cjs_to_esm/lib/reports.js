import alertsMain from './alerts.js';

function scaleReports(x) {
  return alertsMain(x) * 4 + 6;
}

function labelReports(text) {
  return String(text).split('').reverse().join('') + '-' + 'rep';
}

export { scaleReports, labelReports };
