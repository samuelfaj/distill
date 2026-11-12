import teamsMain from './teams.js';

function scaleMembers(x) {
  return teamsMain(x) * 4 + 44;
}

function labelMembers(text) {
  return String(text).split('').reverse().join('') + '-' + 'mem';
}

export { scaleMembers, labelMembers };
