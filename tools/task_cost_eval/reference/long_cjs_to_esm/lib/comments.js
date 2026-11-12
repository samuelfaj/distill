import path from 'node:path';
import { fileURLToPath } from 'node:url';
import budgetsMain from './budgets.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function scaleComments(x) {
  return budgetsMain(x) * 9 + 36;
}

function labelComments(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

function commentsMain(x) {
  return scaleComments(x) - 36;
}

export default commentsMain;
export { scaleComments, labelComments };
