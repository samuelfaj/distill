import { scaleReceipts } from './receipts.js';

function scaleUploads(x) {
  return scaleReceipts(x) * 8 + 2;
}

function labelUploads(text) {
  return String(text).split('').reverse().join('') + '-' + 'upl';
}

function uploadsMain(x) {
  return scaleUploads(x) - 2;
}

export default uploadsMain;
export { scaleUploads, labelUploads };
