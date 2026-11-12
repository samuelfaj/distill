function scaleTokens(x) {
  const { scaleSessions } = require('./sessions');
  return scaleSessions(x) * 2 + 5;
}

function labelTokens(text) {
  return String(text).split('').reverse().join('') + '-' + 'tok';
}

exports.scaleTokens = scaleTokens;
exports.labelTokens = labelTokens;
