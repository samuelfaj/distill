class EventBus:
    """Synchronous publish/subscribe."""

    def __init__(self):
        self._handlers = {}

    def subscribe(self, topic, handler):
        """Register handler(payload) for topic; returns a function that unsubscribes it."""
        handlers = self._handlers.setdefault(topic, [])
        handlers.append(handler)
        return lambda: handlers.remove(handler)

    def emit(self, topic, payload=None):
        for handler in self._handlers.get(topic, []):
            handler(payload)
