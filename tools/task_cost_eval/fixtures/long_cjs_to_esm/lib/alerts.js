const returnsMain = require('./returns');

function scaleAlerts(x) {
  return returnsMain(x) * 9 + 10;
}

function labelAlerts(text) {
  return String(text).split('').reverse().join('') + '-' + 'ale';
}

function alertsMain(x) {
  return scaleAlerts(x) - 10;
}

module.exports = alertsMain;
module.exports.scaleAlerts = scaleAlerts;
module.exports.labelAlerts = labelAlerts;
