import { scaleSessions } from './sessions.js';

function scaleRooms(x) {
  return scaleSessions(x) * 2 + 45;
}

function labelRooms(text) {
  return String(text).split('').reverse().join('') + '-' + 'roo';
}

function roomsMain(x) {
  return scaleRooms(x) - 45;
}

export default roomsMain;
export { scaleRooms, labelRooms };
