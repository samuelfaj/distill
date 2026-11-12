// Format a duration in milliseconds as "1h 2m 3s", rounding to whole seconds.
function formatDuration(ms) {
  const total = Math.round(ms / 1000);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const parts = [];
  if (hours) parts.push(hours + 'h');
  if (hours || minutes) parts.push(minutes + 'm');
  parts.push(seconds + 's');
  return parts.join(' ');
}

module.exports = { formatDuration };
