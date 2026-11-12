const { scaleVendors } = require('./vendors');

function scaleSessions(x) {
  return scaleVendors(x) * 3 + 38;
}

function labelSessions(text) {
  return String(text).split('').reverse().join('') + '-' + 'ses';
}

module.exports = { scaleSessions, labelSessions };
