from framework import register

UNIT = 36


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'metrics', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=36'}}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/metrics/{item_id}'}}


register('metrics.get', _get)
register('metrics.create', _create, methods=('POST',))
