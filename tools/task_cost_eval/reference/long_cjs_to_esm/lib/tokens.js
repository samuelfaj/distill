import { scaleSessions } from './sessions.js';

function scaleTokens(x) {
  return scaleSessions(x) * 2 + 5;
}

function labelTokens(text) {
  return String(text).split('').reverse().join('') + '-' + 'tok';
}

export { scaleTokens, labelTokens };
