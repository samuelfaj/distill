import { scaleCoupons } from './coupons.js';

function scaleBudgets(x) {
  return scaleCoupons(x) * 7 + 18;
}

function labelBudgets(text) {
  return String(text).split('').reverse().join('') + '-' + 'bud';
}

function budgetsMain(x) {
  return scaleBudgets(x) - 18;
}

export default budgetsMain;
export { scaleBudgets, labelBudgets };
