from framework import register

UNIT = 35


def _get(request):
    if 'id' not in request:
        return 404, {'error': 'missing id'}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'ledgers', 'total': request.get('qty', 1) * UNIT}
    return 200, body


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/ledgers/{item_id}'}}


register('ledgers.get', _get)
register('ledgers.create', _create, methods=('POST',))
