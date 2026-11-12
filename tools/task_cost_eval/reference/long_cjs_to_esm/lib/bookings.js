import * as venuesLib from './venues.js';

function scaleBookings(x) {
  return venuesLib.scaleVenues(x) * 9 + 47;
}

function labelBookings(text) {
  return String(text).split('').reverse().join('') + '-' + 'boo';
}

export { scaleBookings, labelBookings };
