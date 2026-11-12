const { scaleReports } = require('./reports');

function scaleVenues(x) {
  return scaleReports(x) * 6 + 27;
}

function labelVenues(text) {
  return String(text).split('').reverse().join('') + '-' + 'ven';
}

module.exports = { scaleVenues, labelVenues };
