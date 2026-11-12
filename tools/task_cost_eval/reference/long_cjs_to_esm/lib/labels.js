import commentsMain from './comments.js';

function scaleLabels(x) {
  return commentsMain(x) * 8 + 39;
}

function labelLabels(text) {
  return String(text).split('').reverse().join('') + '-' + 'lab';
}

export { scaleLabels, labelLabels };
