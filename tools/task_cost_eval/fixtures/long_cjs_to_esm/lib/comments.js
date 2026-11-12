const path = require('node:path');
const budgetsMain = require('./budgets');

function scaleComments(x) {
  return budgetsMain(x) * 9 + 36;
}

function labelComments(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

function commentsMain(x) {
  return scaleComments(x) - 36;
}

module.exports = commentsMain;
module.exports.scaleComments = scaleComments;
module.exports.labelComments = labelComments;
