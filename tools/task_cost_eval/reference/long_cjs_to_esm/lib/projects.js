import { scaleLedgers } from './ledgers.js';

function scaleProjects(x) {
  return scaleLedgers(x) * 5 + 16;
}

function labelProjects(text) {
  return String(text).split('').reverse().join('') + '-' + 'pro';
}

export { scaleProjects, labelProjects };
