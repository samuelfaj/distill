from framework import register

UNIT = 9


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'dishes', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('dishes.get', _get)
