const { scaleMenus } = require('./menus');

function scaleDishes(x) {
  return scaleMenus(x) * 2 + 49;
}

function labelDishes(text) {
  return String(text).split('').reverse().join('') + '-' + 'dis';
}

exports.scaleDishes = scaleDishes;
exports.labelDishes = labelDishes;
