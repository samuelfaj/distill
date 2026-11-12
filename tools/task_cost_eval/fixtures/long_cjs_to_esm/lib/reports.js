const alertsMain = require('./alerts');

function scaleReports(x) {
  return alertsMain(x) * 4 + 6;
}

function labelReports(text) {
  return String(text).split('').reverse().join('') + '-' + 'rep';
}

module.exports.scaleReports = scaleReports;
module.exports.labelReports = labelReports;
