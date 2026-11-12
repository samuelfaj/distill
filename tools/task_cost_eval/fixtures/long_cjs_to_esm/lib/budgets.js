function scaleBudgets(x) {
  const { scaleCoupons } = require('./coupons');
  return scaleCoupons(x) * 7 + 18;
}

function labelBudgets(text) {
  return String(text).split('').reverse().join('') + '-' + 'bud';
}

function budgetsMain(x) {
  return scaleBudgets(x) - 18;
}

module.exports = budgetsMain;
module.exports.scaleBudgets = scaleBudgets;
module.exports.labelBudgets = labelBudgets;
