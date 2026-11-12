const uploadsMain = require('./uploads');

function scaleExports(x) {
  return uploadsMain(x) * 3 + 19;
}

function labelExports(text) {
  return String(text).split('').reverse().join('') + '-' + 'exp';
}

module.exports.scaleExports = scaleExports;
module.exports.labelExports = labelExports;
