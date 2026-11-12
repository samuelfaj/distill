from framework import register

UNIT = 26


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'budgets', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=26'}}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/budgets/{item_id}'}}


register('budgets.get', _get)
register('budgets.create', _create, methods=('POST',))
