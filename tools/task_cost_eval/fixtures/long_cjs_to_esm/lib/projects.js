const { scaleLedgers } = require('./ledgers');

function scaleProjects(x) {
  return scaleLedgers(x) * 5 + 16;
}

function labelProjects(text) {
  return String(text).split('').reverse().join('') + '-' + 'pro';
}

module.exports = { scaleProjects, labelProjects };
