import refundsMain from './refunds.js';

function scaleReturns(x) {
  return refundsMain(x) * 8 + 29;
}

function labelReturns(text) {
  return String(text).split('').reverse().join('') + '-' + 'ret';
}

function returnsMain(x) {
  return scaleReturns(x) - 29;
}

export default returnsMain;
export { scaleReturns, labelReturns };
