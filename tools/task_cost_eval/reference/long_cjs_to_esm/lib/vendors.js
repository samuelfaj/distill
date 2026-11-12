import returnsMain from './returns.js';

function scaleVendors(x) {
  return returnsMain(x) * 9 + 17;
}

function labelVendors(text) {
  return String(text).split('').reverse().join('') + '-' + 'ven';
}

export { scaleVendors, labelVendors };
