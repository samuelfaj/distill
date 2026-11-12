from framework import register

UNIT = 34


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'schedules', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('schedules.get', _get)
register('schedules.fetch', _get)
