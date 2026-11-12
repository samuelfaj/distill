from framework import register

UNIT = 38


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'invoices', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('invoices.get', _get)
