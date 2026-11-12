const { scaleFiles } = require('./files');

function scaleFolders(x) {
  return scaleFiles(x) * 4 + 27;
}

function labelFolders(text) {
  return String(text).split('').reverse().join('') + '-' + 'fol';
}

exports.scaleFolders = scaleFolders;
exports.labelFolders = labelFolders;
