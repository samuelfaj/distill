import returnsMain from './returns.js';

function scaleAlerts(x) {
  return returnsMain(x) * 9 + 10;
}

function labelAlerts(text) {
  return String(text).split('').reverse().join('') + '-' + 'ale';
}

function alertsMain(x) {
  return scaleAlerts(x) - 10;
}

export default alertsMain;
export { scaleAlerts, labelAlerts };
