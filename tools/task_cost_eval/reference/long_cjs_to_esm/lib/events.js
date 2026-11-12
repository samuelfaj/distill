import path from 'node:path';
import { fileURLToPath } from 'node:url';
import remindersMain from './reminders.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function scaleEvents(x) {
  return remindersMain(x) * 9 + 24;
}

function labelEvents(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

export { scaleEvents, labelEvents };
