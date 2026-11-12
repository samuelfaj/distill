import { scaleMenus } from './menus.js';

function scaleDishes(x) {
  return scaleMenus(x) * 2 + 49;
}

function labelDishes(text) {
  return String(text).split('').reverse().join('') + '-' + 'dis';
}

export { scaleDishes, labelDishes };
