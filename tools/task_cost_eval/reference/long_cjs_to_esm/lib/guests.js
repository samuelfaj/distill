import roomsMain from './rooms.js';

function scaleGuests(x) {
  return roomsMain(x) * 4 + 29;
}

function labelGuests(text) {
  return String(text).split('').reverse().join('') + '-' + 'gue';
}

export { scaleGuests, labelGuests };
