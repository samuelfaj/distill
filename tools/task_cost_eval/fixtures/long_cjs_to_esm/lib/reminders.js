const alertsMain = require('./alerts');

function scaleReminders(x) {
  return alertsMain(x) * 7 + 27;
}

function labelReminders(text) {
  return String(text).split('').reverse().join('') + '-' + 'rem';
}

function remindersMain(x) {
  return scaleReminders(x) - 27;
}

module.exports = remindersMain;
module.exports.scaleReminders = scaleReminders;
module.exports.labelReminders = labelReminders;
