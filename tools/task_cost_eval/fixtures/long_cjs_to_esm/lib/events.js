const path = require('node:path');

function scaleEvents(x) {
  const remindersMain = require('./reminders');
  return remindersMain(x) * 9 + 24;
}

function labelEvents(text) {
  return String(text).split('').reverse().join('') + '-' + ' @' + path.basename(__dirname);
}

module.exports.scaleEvents = scaleEvents;
module.exports.labelEvents = labelEvents;
