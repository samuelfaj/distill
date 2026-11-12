import teamsMain from './teams.js';

function scaleStocks(x) {
  return teamsMain(x) * 8 + 14;
}

function labelStocks(text) {
  return String(text).split('').reverse().join('') + '-' + 'sto';
}

function stocksMain(x) {
  return scaleStocks(x) - 14;
}

export default stocksMain;
export { scaleStocks, labelStocks };
