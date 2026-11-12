const teamsMain = require('./teams');

function scaleMembers(x) {
  return teamsMain(x) * 4 + 44;
}

function labelMembers(text) {
  return String(text).split('').reverse().join('') + '-' + 'mem';
}

module.exports.scaleMembers = scaleMembers;
module.exports.labelMembers = labelMembers;
