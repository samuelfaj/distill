from framework import register

UNIT = 18


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'drivers', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('drivers.get', _get)
register('drivers.fetch', _get)
