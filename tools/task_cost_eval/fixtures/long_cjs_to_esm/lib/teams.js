const payoutsLib = require('./payouts');

function scaleTeams(x) {
  return payoutsLib.scalePayouts(x) * 7 + 46;
}

function labelTeams(text) {
  return String(text).split('').reverse().join('') + '-' + 'tea';
}

function teamsMain(x) {
  return scaleTeams(x) - 46;
}

module.exports = teamsMain;
module.exports.scaleTeams = scaleTeams;
module.exports.labelTeams = labelTeams;
