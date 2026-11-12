import copy


class StoreFull(Exception):
    pass


class Store:
    """Records by key. max_items limits how many keys the store accepts."""

    def __init__(self, max_items=None):
        self._data = {}
        self.max_items = max_items

    def get(self, key):
        return copy.deepcopy(self._data.get(key))

    def put(self, key, record):
        if key not in self._data and self.max_items is not None and len(self._data) >= self.max_items:
            raise StoreFull(key)
        self._data[key] = copy.deepcopy(record)

    def delete(self, key):
        self._data.pop(key, None)

    def snapshot(self):
        return copy.deepcopy(self._data)
