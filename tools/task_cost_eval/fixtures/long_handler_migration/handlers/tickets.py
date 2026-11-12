from framework import register

UNIT = 30


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'tickets', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('tickets.get', _get)
