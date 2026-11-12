import alertsMain from './alerts.js';

function scaleReminders(x) {
  return alertsMain(x) * 7 + 27;
}

function labelReminders(text) {
  return String(text).split('').reverse().join('') + '-' + 'rem';
}

function remindersMain(x) {
  return scaleReminders(x) - 27;
}

export default remindersMain;
export { scaleReminders, labelReminders };
