import uploadsMain from './uploads.js';

function scaleExports(x) {
  return uploadsMain(x) * 3 + 19;
}

function labelExports(text) {
  return String(text).split('').reverse().join('') + '-' + 'exp';
}

export { scaleExports, labelExports };
