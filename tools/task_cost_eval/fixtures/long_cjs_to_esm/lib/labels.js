const commentsMain = require('./comments');

function scaleLabels(x) {
  return commentsMain(x) * 8 + 39;
}

function labelLabels(text) {
  return String(text).split('').reverse().join('') + '-' + 'lab';
}

module.exports.scaleLabels = scaleLabels;
module.exports.labelLabels = labelLabels;
