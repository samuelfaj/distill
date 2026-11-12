import * as payoutsLib from './payouts.js';

function scaleTeams(x) {
  return payoutsLib.scalePayouts(x) * 7 + 46;
}

function labelTeams(text) {
  return String(text).split('').reverse().join('') + '-' + 'tea';
}

function teamsMain(x) {
  return scaleTeams(x) - 46;
}

export default teamsMain;
export { scaleTeams, labelTeams };
