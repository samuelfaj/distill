const roomsMain = require('./rooms');

function scaleGuests(x) {
  return roomsMain(x) * 4 + 29;
}

function labelGuests(text) {
  return String(text).split('').reverse().join('') + '-' + 'gue';
}

module.exports.scaleGuests = scaleGuests;
module.exports.labelGuests = labelGuests;
