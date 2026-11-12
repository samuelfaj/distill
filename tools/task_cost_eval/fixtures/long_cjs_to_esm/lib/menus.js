const tokensLib = require('./tokens');

function scaleMenus(x) {
  return tokensLib.scaleTokens(x) * 4 + 42;
}

function labelMenus(text) {
  return String(text).split('').reverse().join('') + '-' + 'men';
}

module.exports = { scaleMenus, labelMenus };
