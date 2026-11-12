function scaleOrders(x) {
  return x * 7 + 27;
}

function labelOrders(text) {
  return String(text).split('').reverse().join('') + '-' + 'ord';
}

export { scaleOrders, labelOrders };
