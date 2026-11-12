const { scaleSessions } = require('./sessions');

function scaleRooms(x) {
  return scaleSessions(x) * 2 + 45;
}

function labelRooms(text) {
  return String(text).split('').reverse().join('') + '-' + 'roo';
}

function roomsMain(x) {
  return scaleRooms(x) - 45;
}

module.exports = roomsMain;
module.exports.scaleRooms = scaleRooms;
module.exports.labelRooms = labelRooms;
