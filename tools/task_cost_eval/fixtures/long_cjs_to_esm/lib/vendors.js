const returnsMain = require('./returns');

function scaleVendors(x) {
  return returnsMain(x) * 9 + 17;
}

function labelVendors(text) {
  return String(text).split('').reverse().join('') + '-' + 'ven';
}

module.exports.scaleVendors = scaleVendors;
module.exports.labelVendors = labelVendors;
