import { scaleImports } from './imports.js';

function scaleSchedules(x) {
  return scaleImports(x) * 3 + 5;
}

function labelSchedules(text) {
  return String(text).split('').reverse().join('') + '-' + 'sch';
}

export { scaleSchedules, labelSchedules };
