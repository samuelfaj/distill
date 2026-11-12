const venuesLib = require('./venues');

function scaleBookings(x) {
  return venuesLib.scaleVenues(x) * 9 + 47;
}

function labelBookings(text) {
  return String(text).split('').reverse().join('') + '-' + 'boo';
}

exports.scaleBookings = scaleBookings;
exports.labelBookings = labelBookings;
