import * as projectsLib from './projects.js';

function scaleTasks(x) {
  return projectsLib.scaleProjects(x) * 7 + 29;
}

function labelTasks(text) {
  return String(text).split('').reverse().join('') + '-' + 'tas';
}

export { scaleTasks, labelTasks };
