const { scaleImports } = require('./imports');

function scaleSchedules(x) {
  return scaleImports(x) * 3 + 5;
}

function labelSchedules(text) {
  return String(text).split('').reverse().join('') + '-' + 'sch';
}

exports.scaleSchedules = scaleSchedules;
exports.labelSchedules = labelSchedules;
