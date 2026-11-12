const { scaleCoupons } = require('./coupons');

function scaleShipments(x) {
  return scaleCoupons(x) * 6 + 21;
}

function labelShipments(text) {
  return String(text).split('').reverse().join('') + '-' + 'shi';
}

exports.scaleShipments = scaleShipments;
exports.labelShipments = labelShipments;
