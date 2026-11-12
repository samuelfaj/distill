from framework import register

UNIT = 20


def _get(request):
    if 'id' not in request:
        return {'status': 404, 'body': {'error': 'missing id'}}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'orders', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('orders.get', _get)
