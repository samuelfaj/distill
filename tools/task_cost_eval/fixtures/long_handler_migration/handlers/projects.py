from framework import register

UNIT = 28


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'projects', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/projects/{item_id}'}}


register('projects.get', _get)
register('projects.create', _create, methods=('POST',))
