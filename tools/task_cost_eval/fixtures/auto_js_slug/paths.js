const { slugify } = require('./slug');

function articlePath(title) {
  return '/articles/' + slugify(title);
}

function tagPath(tag) {
  return '/tags/' + slugify(tag);
}

module.exports = { articlePath, tagPath };
