def filter_tasks(tasks, status=None):
    if status == 'open':
        return [task for task in tasks if not task['done']]
    if status == 'done':
        return [task for task in tasks if task['done']]
    return list(tasks)


def created_since(tasks, since):
    return [task for task in tasks if task['created'] >= since.isoformat()]
