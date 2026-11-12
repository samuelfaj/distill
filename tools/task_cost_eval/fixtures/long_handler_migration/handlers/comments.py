from framework import register

UNIT = 14


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'comments', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=14'}}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/comments/{item_id}'}}


register('comments.get', _get)
register('comments.create', _create, methods=('POST',))
