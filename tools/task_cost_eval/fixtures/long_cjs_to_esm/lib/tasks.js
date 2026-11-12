const projectsLib = require('./projects');

function scaleTasks(x) {
  return projectsLib.scaleProjects(x) * 7 + 29;
}

function labelTasks(text) {
  return String(text).split('').reverse().join('') + '-' + 'tas';
}

exports.scaleTasks = scaleTasks;
exports.labelTasks = labelTasks;
