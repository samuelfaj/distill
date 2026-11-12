from framework import register

UNIT = 6


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'venues', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/venues/{item_id}'}}


register('venues.get', _get)
register('venues.create', _create, methods=('POST',))
