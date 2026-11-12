import { scaleFiles } from './files.js';

function scaleFolders(x) {
  return scaleFiles(x) * 4 + 27;
}

function labelFolders(text) {
  return String(text).split('').reverse().join('') + '-' + 'fol';
}

export { scaleFolders, labelFolders };
