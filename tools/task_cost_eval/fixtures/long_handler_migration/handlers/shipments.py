from framework import register

UNIT = 33


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'shipments', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('shipments.get', _get)
register('shipments.fetch', _get)
