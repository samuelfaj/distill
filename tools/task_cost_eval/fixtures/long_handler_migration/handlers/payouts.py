from framework import register

UNIT = 34


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'payouts', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/payouts/{item_id}'}}


register('payouts.get', _get)
register('payouts.create', _create, methods=('POST',))
