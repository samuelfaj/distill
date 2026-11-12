function slugify(text, sep = '_') {
  return String(text)
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, sep)
    .split(sep)
    .filter(Boolean)
    .join(sep);
}

module.exports = { slugify };
