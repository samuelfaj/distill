// Format a duration in milliseconds as "1h 2m 3s", rounding to whole seconds.
function formatDuration(ms) {
  const hours = Math.floor(ms / 3600000);
  const minutes = Math.floor((ms % 3600000) / 60000);
  const seconds = Math.round((ms % 60000) / 1000);
  const parts = [];
  if (hours) parts.push(hours + 'h');
  if (hours || minutes) parts.push(minutes + 'm');
  parts.push(seconds + 's');
  return parts.join(' ');
}

module.exports = { formatDuration };
