from framework import register

UNIT = 12


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'plans', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/plans/{item_id}'}}


register('plans.get', _get)
register('plans.create', _create, methods=('POST',))
